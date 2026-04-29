use rusqlite::{Connection, params};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InodeRow {
    pub ino: u64,
    pub parent_ino: u64,
    pub name: String,
    pub node_uid: Option<String>,
    pub volume_id: Option<String>,
    pub link_id: Option<String>,
    pub is_dir: bool,
    pub size: u64,
    pub media_type: String,
    pub revision_uid: Option<String>,
    pub mtime: i64,
    pub ctime: i64,
    pub cached_path: Option<String>,
    pub dirty: bool,
    pub children_populated: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct JournalEntry {
    pub id: i64,
    pub created_at: i64,
    pub event_type: String,
    pub ino: u64,
    pub payload: String,
    pub status: String,
    pub error: Option<String>,
    pub retry_count: i32,
}

pub struct FuseDb {
    conn: Connection,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl FuseDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS inodes (
                ino                 INTEGER PRIMARY KEY AUTOINCREMENT,
                parent_ino          INTEGER NOT NULL,
                name                TEXT NOT NULL,
                node_uid            TEXT,
                volume_id           TEXT,
                link_id             TEXT,
                is_dir              INTEGER NOT NULL DEFAULT 0,
                size                INTEGER NOT NULL DEFAULT 0,
                media_type          TEXT NOT NULL DEFAULT '',
                revision_uid        TEXT,
                mtime               INTEGER NOT NULL,
                ctime               INTEGER NOT NULL,
                cached_path         TEXT,
                dirty               INTEGER NOT NULL DEFAULT 0,
                children_populated  INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_inodes_parent
                ON inodes(parent_ino, name);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_inodes_node_uid
                ON inodes(node_uid) WHERE node_uid IS NOT NULL;

            CREATE TABLE IF NOT EXISTS journal (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at  INTEGER NOT NULL,
                event_type  TEXT NOT NULL,
                ino         INTEGER NOT NULL,
                payload     TEXT NOT NULL,
                status      TEXT NOT NULL DEFAULT 'pending',
                error       TEXT,
                retry_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_journal_status
                ON journal(status, created_at);

            CREATE TABLE IF NOT EXISTS event_cursors (
                scope       TEXT PRIMARY KEY,
                event_id    TEXT NOT NULL
            );
            ",
        )?;
        Ok(())
    }

    // ── Inode queries ──────────────────────────────────────────────────

    pub fn get_inode(&self, ino: u64) -> Option<InodeRow> {
        self.conn
            .prepare_cached(
                "SELECT ino, parent_ino, name, node_uid, volume_id, link_id, is_dir, size,
                        media_type, revision_uid, mtime, ctime, cached_path, dirty,
                        children_populated
                 FROM inodes WHERE ino = ?",
            )
            .ok()?
            .query_row(params![ino as i64], Self::map_row)
            .ok()
    }

    pub fn lookup_child(&self, parent_ino: u64, name: &str) -> Option<InodeRow> {
        self.conn
            .prepare_cached(
                "SELECT ino, parent_ino, name, node_uid, volume_id, link_id, is_dir, size,
                        media_type, revision_uid, mtime, ctime, cached_path, dirty,
                        children_populated
                 FROM inodes WHERE parent_ino = ? AND name = ?",
            )
            .ok()?
            .query_row(params![parent_ino as i64, name], Self::map_row)
            .ok()
    }

    pub fn list_children(&self, parent_ino: u64) -> Vec<InodeRow> {
        (|| -> rusqlite::Result<Vec<InodeRow>> {
            let mut stmt = self.conn.prepare_cached(
                "SELECT ino, parent_ino, name, node_uid, volume_id, link_id, is_dir, size,
                        media_type, revision_uid, mtime, ctime, cached_path, dirty,
                        children_populated
                 FROM inodes WHERE parent_ino = ? AND ino != parent_ino ORDER BY name",
            )?;
            let rows = stmt.query_map(params![parent_ino as i64], Self::map_row)?;
            rows.collect()
        })()
        .unwrap_or_default()
    }

    /// Insert a new inode (auto-increment ino). Returns the allocated ino.
    pub fn insert_inode(
        &self,
        parent_ino: u64,
        name: &str,
        node_uid: Option<&str>,
        volume_id: Option<&str>,
        link_id: Option<&str>,
        is_dir: bool,
        size: u64,
        media_type: &str,
        revision_uid: Option<&str>,
        mtime: i64,
    ) -> anyhow::Result<u64> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO inodes
                (parent_ino, name, node_uid, volume_id, link_id, is_dir, size,
                 media_type, revision_uid, mtime, ctime, dirty, children_populated)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0)",
            params![
                parent_ino as i64,
                name,
                node_uid,
                volume_id,
                link_id,
                is_dir as i64,
                size as i64,
                media_type,
                revision_uid,
                mtime,
                now,
            ],
        )?;
        Ok(self.conn.last_insert_rowid() as u64)
    }

    /// Insert the root inode (ino = 1). No-op if row already exists.
    pub fn insert_root(
        &self,
        node_uid: Option<&str>,
        volume_id: Option<&str>,
        link_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let now = now_unix();
        self.conn.execute(
            "INSERT OR IGNORE INTO inodes
                (ino, parent_ino, name, node_uid, volume_id, link_id,
                 is_dir, size, media_type, mtime, ctime, dirty, children_populated)
             VALUES (1, 1, '', ?, ?, ?, 1, 0, '', ?, ?, 0, 0)",
            params![node_uid, volume_id, link_id, now, now],
        )?;
        Ok(())
    }

    pub fn update_node_uid(
        &self,
        ino: u64,
        node_uid: &str,
        volume_id: &str,
        link_id: &str,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE inodes SET node_uid = ?, volume_id = ?, link_id = ? WHERE ino = ?",
            params![node_uid, volume_id, link_id, ino as i64],
        )?;
        Ok(())
    }

    pub fn set_cached_path(&self, ino: u64, path: Option<&str>) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE inodes SET cached_path = ? WHERE ino = ?",
            params![path, ino as i64],
        )?;
        Ok(())
    }

    pub fn set_dirty(&self, ino: u64, dirty: bool) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE inodes SET dirty = ? WHERE ino = ?",
            params![dirty as i64, ino as i64],
        )?;
        Ok(())
    }

    pub fn set_children_populated(&self, ino: u64) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE inodes SET children_populated = 1 WHERE ino = ?",
            params![ino as i64],
        )?;
        Ok(())
    }

    pub fn clear_children_populated(&self, ino: u64) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE inodes SET children_populated = 0 WHERE ino = ?",
            params![ino as i64],
        )?;
        Ok(())
    }

    pub fn delete_inode(&self, ino: u64) -> anyhow::Result<()> {
        self.conn
            .execute("DELETE FROM inodes WHERE ino = ?", params![ino as i64])?;
        Ok(())
    }

    pub fn rename_inode(&self, ino: u64, new_parent: u64, new_name: &str) -> anyhow::Result<()> {
        let now = now_unix();
        self.conn.execute(
            "UPDATE inodes SET parent_ino = ?, name = ?, ctime = ? WHERE ino = ?",
            params![new_parent as i64, new_name, now, ino as i64],
        )?;
        Ok(())
    }

    pub fn update_size(&self, ino: u64, size: u64) -> anyhow::Result<()> {
        let now = now_unix();
        self.conn.execute(
            "UPDATE inodes SET size = ?, mtime = ? WHERE ino = ?",
            params![size as i64, now, ino as i64],
        )?;
        Ok(())
    }

    pub fn update_revision(
        &self,
        ino: u64,
        revision_uid: &str,
        size: u64,
    ) -> anyhow::Result<()> {
        let now = now_unix();
        self.conn.execute(
            "UPDATE inodes SET revision_uid = ?, size = ?, mtime = ?,
                    cached_path = NULL WHERE ino = ?",
            params![revision_uid, size as i64, now, ino as i64],
        )?;
        Ok(())
    }

    pub fn find_by_node_uid(&self, node_uid: &str) -> Option<InodeRow> {
        self.conn
            .prepare_cached(
                "SELECT ino, parent_ino, name, node_uid, volume_id, link_id, is_dir, size,
                        media_type, revision_uid, mtime, ctime, cached_path, dirty,
                        children_populated
                 FROM inodes WHERE node_uid = ?",
            )
            .ok()?
            .query_row(params![node_uid], Self::map_row)
            .ok()
    }

    pub fn find_by_link_id(&self, link_id: &str) -> Option<InodeRow> {
        self.conn
            .prepare_cached(
                "SELECT ino, parent_ino, name, node_uid, volume_id, link_id, is_dir, size,
                        media_type, revision_uid, mtime, ctime, cached_path, dirty,
                        children_populated
                 FROM inodes WHERE link_id = ?",
            )
            .ok()?
            .query_row(params![link_id], Self::map_row)
            .ok()
    }

    // ── Journal ──────────────────────────────────────────────────────

    pub fn enqueue_journal(
        &self,
        event_type: &str,
        ino: u64,
        payload: &str,
    ) -> anyhow::Result<i64> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO journal (created_at, event_type, ino, payload, status)
             VALUES (?, ?, ?, ?, 'pending')",
            params![now, event_type, ino as i64, payload],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn load_pending_journal(&self, limit: i64) -> Vec<JournalEntry> {
        (|| -> rusqlite::Result<Vec<JournalEntry>> {
            let mut stmt = self.conn.prepare_cached(
                "SELECT id, created_at, event_type, ino, payload, status, error, retry_count
                 FROM journal WHERE status = 'pending' ORDER BY created_at ASC LIMIT ?",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                Ok(JournalEntry {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    event_type: row.get(2)?,
                    ino: row.get::<_, i64>(3)? as u64,
                    payload: row.get(4)?,
                    status: row.get(5)?,
                    error: row.get(6)?,
                    retry_count: row.get(7)?,
                })
            })?;
            rows.collect()
        })()
        .unwrap_or_default()
    }

    pub fn update_journal_status(
        &self,
        id: i64,
        status: &str,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE journal SET status = ?, error = ?, retry_count = retry_count + 1
             WHERE id = ?",
            params![status, error, id],
        )?;
        Ok(())
    }

    pub fn delete_completed_journal(&self) -> anyhow::Result<usize> {
        let n = self
            .conn
            .execute("DELETE FROM journal WHERE status IN ('done', 'failed')", [])?;
        Ok(n)
    }

    // ── Event cursors ────────────────────────────────────────────────

    pub fn get_event_cursor(&self, scope: &str) -> Option<String> {
        self.conn
            .prepare_cached("SELECT event_id FROM event_cursors WHERE scope = ?")
            .ok()?
            .query_row(params![scope], |row| row.get(0))
            .ok()
    }

    pub fn set_event_cursor(&self, scope: &str, event_id: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO event_cursors (scope, event_id) VALUES (?, ?)",
            params![scope, event_id],
        )?;
        Ok(())
    }

    // ── helpers ──────────────────────────────────────────────────────

    fn map_row(row: &rusqlite::Row) -> rusqlite::Result<InodeRow> {
        Ok(InodeRow {
            ino: row.get::<_, i64>(0)? as u64,
            parent_ino: row.get::<_, i64>(1)? as u64,
            name: row.get(2)?,
            node_uid: row.get(3)?,
            volume_id: row.get(4)?,
            link_id: row.get(5)?,
            is_dir: row.get::<_, i64>(6)? != 0,
            size: row.get::<_, i64>(7)? as u64,
            media_type: row.get(8)?,
            revision_uid: row.get(9)?,
            mtime: row.get(10)?,
            ctime: row.get(11)?,
            cached_path: row.get(12)?,
            dirty: row.get::<_, i64>(13)? != 0,
            children_populated: row.get::<_, i64>(14)? != 0,
        })
    }
}