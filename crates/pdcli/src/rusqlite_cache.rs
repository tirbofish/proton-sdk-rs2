use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use proton_drive_sdk::node::Node;
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
        
        // Ensure all nodes have inodes
        let _ = conn.execute("UPDATE nodes SET inode = rowid + 100 WHERE inode IS NULL", []);

        conn.execute("CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes (volume_id, parent_link_id)", [])?;

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
            let size = match node {
                Node::File(f) | Node::Photo(f) => Some(f.total_size_on_cloud_storage),
                _ => None,
            };

            tx.execute(
                "INSERT INTO nodes (
                    volume_id, link_id, parent_link_id, name, node_type, 
                    creation_time, modification_time, is_trashed, size, inode
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 
                    COALESCE(
                        (SELECT inode FROM nodes WHERE volume_id = ?1 AND link_id = ?2),
                        (SELECT IFNULL(MAX(inode), 100) + 1 FROM nodes)
                    )
                )
                ON CONFLICT(volume_id, link_id) DO UPDATE SET
                    parent_link_id = excluded.parent_link_id,
                    name = excluded.name,
                    node_type = excluded.node_type,
                    creation_time = excluded.creation_time,
                    modification_time = excluded.modification_time,
                    is_trashed = excluded.is_trashed,
                    size = excluded.size",
                params![
                    base.uid.volume_id.raw(),
                    base.uid.link_id.raw(),
                    base.parent_uid.as_ref().map(|u| u.link_id.raw()),
                    base.name,
                    format!("{:?}", node.ty()),
                    base.creation_time.to_rfc3339(),
                    Utc::now().to_rfc3339(),
                    if *is_trashed { 1 } else { 0 },
                    size,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn get_node_by_uid(&self, volume_id: &VolumeId, link_id: &LinkId) -> Result<Option<CachedNode>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode 
             FROM nodes WHERE volume_id = ?1 AND link_id = ?2",
            params![volume_id.raw(), link_id.raw()],
            |row| {
                Ok(CachedNode {
                    volume_id: row.get(0)?,
                    link_id: row.get(1)?,
                    parent_link_id: row.get(2)?,
                    name: row.get(3)?,
                    node_type: row.get(4)?,
                    creation_time: row.get::<_, String>(5)?.parse().unwrap_or(Utc::now()),
                    modification_time: row.get::<_, String>(6)?.parse().unwrap_or(Utc::now()),
                    is_trashed: row.get::<_, i32>(7)? != 0,
                    size: row.get(8)?,
                    inode: row.get(9)?,
                })
            },
        ).optional().map_err(Into::into)
    }

    pub fn get_node_by_inode(&self, inode: u64) -> Result<Option<CachedNode>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode 
             FROM nodes WHERE inode = ?1",
            params![inode],
            |row| {
                Ok(CachedNode {
                    volume_id: row.get(0)?,
                    link_id: row.get(1)?,
                    parent_link_id: row.get(2)?,
                    name: row.get(3)?,
                    node_type: row.get(4)?,
                    creation_time: row.get::<_, String>(5)?.parse().unwrap_or(Utc::now()),
                    modification_time: row.get::<_, String>(6)?.parse().unwrap_or(Utc::now()),
                    is_trashed: row.get::<_, i32>(7)? != 0,
                    size: row.get(8)?,
                    inode: row.get(9)?,
                })
            },
        ).optional().map_err(Into::into)
    }

    pub fn get_child_by_name(&self, volume_id: &VolumeId, parent_link_id: Option<&LinkId>, name: &str) -> Result<Option<CachedNode>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode 
             FROM nodes WHERE volume_id = ?1 AND parent_link_id IS ?2 AND name = ?3",
            params![volume_id.raw(), parent_link_id.map(|id| id.raw()), name],
            |row| {
                Ok(CachedNode {
                    volume_id: row.get(0)?,
                    link_id: row.get(1)?,
                    parent_link_id: row.get(2)?,
                    name: row.get(3)?,
                    node_type: row.get(4)?,
                    creation_time: row.get::<_, String>(5)?.parse().unwrap_or(Utc::now()),
                    modification_time: row.get::<_, String>(6)?.parse().unwrap_or(Utc::now()),
                    is_trashed: row.get::<_, i32>(7)? != 0,
                    size: row.get(8)?,
                    inode: row.get(9)?,
                })
            },
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

    pub fn list_trash(&self, volume_id: &VolumeId) -> Result<Vec<CachedNode>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode 
             FROM nodes WHERE volume_id = ?1 AND is_trashed = 1"
        )?;

        let rows = stmt.query_map(
            params![volume_id.raw()],
            |row| {
                Ok(CachedNode {
                    volume_id: row.get(0)?,
                    link_id: row.get(1)?,
                    parent_link_id: row.get(2)?,
                    name: row.get(3)?,
                    node_type: row.get(4)?,
                    creation_time: row.get::<_, String>(5)?.parse().unwrap_or(Utc::now()),
                    modification_time: row.get::<_, String>(6)?.parse().unwrap_or(Utc::now()),
                    is_trashed: row.get::<_, i32>(7)? != 0,
                    size: row.get(8)?,
                    inode: row.get(9)?,
                })
            },
        )?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
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
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode 
             FROM nodes WHERE volume_id = ?1 AND parent_link_id IS ?2 AND is_trashed = 0"
        )?;

        let rows = stmt.query_map(
            params![volume_id.raw(), parent_link_id.map(|id| id.raw())],
            |row| {
                Ok(CachedNode {
                    volume_id: row.get(0)?,
                    link_id: row.get(1)?,
                    parent_link_id: row.get(2)?,
                    name: row.get(3)?,
                    node_type: row.get(4)?,
                    creation_time: row.get::<_, String>(5)?.parse().unwrap_or(Utc::now()),
                    modification_time: row.get::<_, String>(6)?.parse().unwrap_or(Utc::now()),
                    is_trashed: row.get::<_, i32>(7)? != 0,
                    size: row.get(8)?,
                    inode: row.get(9)?,
                })
            },
        )?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
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
}
