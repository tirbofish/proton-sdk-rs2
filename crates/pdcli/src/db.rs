use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncEvent {
    pub id: i64,
    pub created_at: i64,
    pub source: String,
    pub event_type: String,
    pub name: Option<String>,
    pub detail: Option<String>,
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
        Self::open_with_key(path, &crate::credentials::cache_master_key()?)
    }

    pub fn open_with_key(path: &Path, key: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(key.len() == 32, "fuse db key must be 32 bytes");
        if looks_like_plaintext_sqlite(path) {
            migrate_plaintext_to_sqlcipher(path, key)?;
        }
        let conn = Connection::open(path)?;
        apply_sqlcipher_key(&conn, key)?;
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        })?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        crate::credentials::restrict_permissions(path, 0o600);
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

            CREATE TABLE IF NOT EXISTS sync_events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at  INTEGER NOT NULL,
                source      TEXT NOT NULL,
                event_type  TEXT NOT NULL,
                name        TEXT,
                detail      TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_sync_events_created_at
                ON sync_events(created_at DESC);
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

    pub fn cached_file_inodes(&self) -> Vec<(u64, String)> {
        (|| -> rusqlite::Result<Vec<(u64, String)>> {
            let mut stmt = self.conn.prepare_cached(
                "SELECT ino, cached_path
                 FROM inodes
                 WHERE is_dir = 0 AND cached_path IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |row| {
                let ino: i64 = row.get(0)?;
                let cached_path: String = row.get(1)?;
                Ok((ino as u64, cached_path))
            })?;
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

    pub fn ensure_my_files_root(&self) -> anyhow::Result<u64> {
        self.insert_root(None, None, None)?;
        if let Some(row) = self.lookup_child(1, "MyFiles") {
            if let Some(root) = self.get_inode(1) {
                if root.node_uid.is_some() || root.volume_id.is_some() || root.link_id.is_some() {
                    self.conn.execute(
                        "UPDATE inodes
                         SET node_uid = NULL, volume_id = NULL, link_id = NULL
                         WHERE ino = 1",
                        [],
                    )?;
                    self.conn.execute(
                        "UPDATE inodes
                         SET node_uid = ?, volume_id = ?, link_id = ?
                         WHERE ino = ? AND node_uid IS NULL",
                        params![root.node_uid, root.volume_id, root.link_id, row.ino as i64],
                    )?;
                }
            }
            self.conn.execute(
                "UPDATE inodes
                 SET parent_ino = ?
                 WHERE parent_ino = 1 AND ino NOT IN (1, ?)",
                params![row.ino as i64, row.ino as i64],
            )?;
            return Ok(row.ino);
        }

        let old_root = self.get_inode(1);
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO inodes
                (parent_ino, name, node_uid, volume_id, link_id, is_dir, size,
                 media_type, mtime, ctime, dirty, children_populated)
             VALUES (1, 'MyFiles', NULL, NULL, NULL, 1, 0, '', ?, ?, 0, 0)",
            params![now, now],
        )?;
        let my_files_ino = self.conn.last_insert_rowid() as u64;

        if let Some(root) = old_root {
            if root.node_uid.is_some() || root.volume_id.is_some() || root.link_id.is_some() {
                self.conn.execute(
                    "UPDATE inodes
                     SET node_uid = NULL, volume_id = NULL, link_id = NULL
                     WHERE ino = 1",
                    [],
                )?;
                self.conn.execute(
                    "UPDATE inodes
                     SET node_uid = ?, volume_id = ?, link_id = ?
                     WHERE ino = ?",
                    params![
                        root.node_uid,
                        root.volume_id,
                        root.link_id,
                        my_files_ino as i64
                    ],
                )?;
            }
        }

        self.conn.execute(
            "UPDATE inodes
             SET parent_ino = ?
             WHERE parent_ino = 1 AND ino NOT IN (1, ?)",
            params![my_files_ino as i64, my_files_ino as i64],
        )?;

        Ok(my_files_ino)
    }

    pub fn my_files_inode(&self) -> Option<InodeRow> {
        self.lookup_child(1, "MyFiles")
    }

    pub fn inode_path(&self, ino: u64) -> Option<Vec<InodeRow>> {
        let mut rows = Vec::new();
        let mut current = self.get_inode(ino)?;

        loop {
            let parent_ino = current.parent_ino;
            let current_ino = current.ino;
            rows.push(current);
            if current_ino == 1 || current_ino == parent_ino {
                break;
            }
            current = self.get_inode(parent_ino)?;
        }

        rows.reverse();
        Some(rows)
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

    pub fn dirty_files(&self) -> Vec<InodeRow> {
        (|| -> rusqlite::Result<Vec<InodeRow>> {
            let mut stmt = self.conn.prepare_cached(
                "SELECT ino, parent_ino, name, node_uid, volume_id, link_id, is_dir, size,
                        media_type, revision_uid, mtime, ctime, cached_path, dirty,
                        children_populated
                 FROM inodes WHERE dirty = 1 AND is_dir = 0",
            )?;
            let rows = stmt.query_map([], Self::map_row)?;
            rows.collect()
        })()
        .unwrap_or_default()
    }

    pub fn has_pending_journal(&self, ino: u64) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM journal WHERE ino = ? AND status = 'pending' LIMIT 1",
                params![ino as i64],
                |_| Ok(()),
            )
            .is_ok()
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

    pub fn update_size_only(&self, ino: u64, size: u64) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE inodes SET size = ? WHERE ino = ?",
            params![size as i64, ino as i64],
        )?;
        Ok(())
    }

    pub fn update_remote_metadata(
        &self,
        ino: u64,
        size: u64,
        media_type: &str,
        revision_uid: Option<&str>,
        mtime: i64,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE inodes
             SET size = ?,
                 media_type = ?,
                 cached_path = CASE
                     WHEN revision_uid IS NOT ? THEN NULL
                     ELSE cached_path
                 END,
                 revision_uid = ?,
                 mtime = ?
             WHERE ino = ?",
            params![
                size as i64,
                media_type,
                revision_uid,
                revision_uid,
                mtime,
                ino as i64
            ],
        )?;
        Ok(())
    }

    pub fn update_revision(&self, ino: u64, revision_uid: &str, size: u64) -> anyhow::Result<()> {
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
                 FROM journal
                 WHERE status = 'pending'
                   AND retry_count = (
                       SELECT MIN(retry_count) FROM journal WHERE status = 'pending'
                   )
                 ORDER BY created_at ASC LIMIT ?",
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

    // ── Sync events ──────────────────────────────────────────────────

    pub fn record_sync_event(
        &self,
        source: &str,
        event_type: &str,
        name: Option<&str>,
        detail: Option<&str>,
    ) -> anyhow::Result<()> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO sync_events (created_at, source, event_type, name, detail)
             VALUES (?, ?, ?, ?, ?)",
            params![now, source, event_type, name, detail],
        )?;
        self.conn.execute(
            "DELETE FROM sync_events
             WHERE id NOT IN (
                 SELECT id FROM sync_events ORDER BY created_at DESC, id DESC LIMIT 100
             )",
            [],
        )?;
        Ok(())
    }

    pub fn recent_sync_events(&self, limit: i64) -> Vec<SyncEvent> {
        (|| -> rusqlite::Result<Vec<SyncEvent>> {
            let mut stmt = self.conn.prepare_cached(
                "SELECT id, created_at, source, event_type, name, detail
                 FROM sync_events ORDER BY created_at DESC, id DESC LIMIT ?",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                Ok(SyncEvent {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    source: row.get(2)?,
                    event_type: row.get(3)?,
                    name: row.get(4)?,
                    detail: row.get(5)?,
                })
            })?;
            rows.collect()
        })()
        .unwrap_or_default()
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

fn sqlcipher_key_literal(key: &[u8]) -> String {
    format!(
        "x'{}'",
        key.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

fn apply_sqlcipher_key(conn: &Connection, key: &[u8]) -> anyhow::Result<()> {
    conn.execute_batch(&format!("PRAGMA key = \"{}\";", sqlcipher_key_literal(key)))?;
    Ok(())
}

fn looks_like_plaintext_sqlite(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hdr = [0u8; 16];
    std::io::Read::read_exact(&mut file, &mut hdr).is_ok() && hdr.starts_with(b"SQLite format 3")
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn sql_escape_path(path: &Path) -> anyhow::Result<String> {
    let s = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("fuse db path is not valid UTF-8"))?;
    Ok(s.replace('\'', "''"))
}

fn migrate_plaintext_to_sqlcipher(path: &Path, key: &[u8]) -> anyhow::Result<()> {
    let tmp = sqlite_sidecar(path, ".tmp-enc");
    let _ = std::fs::remove_file(&tmp);
    {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        conn.execute_batch(&format!(
            "ATTACH DATABASE '{}' AS encrypted KEY \"{}\";",
            sql_escape_path(&tmp)?,
            sqlcipher_key_literal(key)
        ))?;
        conn.query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()))?;
        conn.execute_batch("DETACH DATABASE encrypted;")?;
    }
    std::fs::rename(&tmp, path)?;
    let _ = std::fs::remove_file(sqlite_sidecar(path, "-wal"));
    let _ = std::fs::remove_file(sqlite_sidecar(path, "-shm"));
    tracing::info!(path = %path.display(), "migrated fuse.db to SQLCipher");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pdcli-fuse-enc-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sqlite_sidecar(path, "-wal"));
        let _ = std::fs::remove_file(sqlite_sidecar(path, "-shm"));
    }

    #[test]
    fn fuse_db_encrypted_at_rest() {
        let path = temp_db();
        let key = [7u8; 32];
        {
            let db = FuseDb::open_with_key(&path, &key).unwrap();
            db.enqueue_journal("create_file", 1, r#"{"name":"secret.txt"}"#)
                .unwrap();
        }
        let hdr = std::fs::read(&path).unwrap();
        assert!(
            !hdr.starts_with(b"SQLite format 3"),
            "fuse.db still looks like plaintext SQLite"
        );
        let db = FuseDb::open_with_key(&path, &key).unwrap();
        let entries = db.load_pending_journal(10);
        assert_eq!(entries[0].payload, r#"{"name":"secret.txt"}"#);
        cleanup(&path);
    }

    #[test]
    fn migrates_plaintext_fuse_db() {
        let path = temp_db();
        let key = [9u8; 32];
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE journal (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at INTEGER NOT NULL,
                    event_type TEXT NOT NULL,
                    ino INTEGER NOT NULL,
                    payload TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    error TEXT,
                    retry_count INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO journal (created_at, event_type, ino, payload, status)
                 VALUES (1, 'create_file', 2, '{\"name\":\"old.txt\"}', 'pending')",
                [],
            )
            .unwrap();
        }
        assert!(looks_like_plaintext_sqlite(&path));
        let db = FuseDb::open_with_key(&path, &key).unwrap();
        let entries = db.load_pending_journal(10);
        assert_eq!(entries[0].payload, r#"{"name":"old.txt"}"#);
        drop(db);
        let hdr = std::fs::read(&path).unwrap();
        assert!(!hdr.starts_with(b"SQLite format 3"));
        cleanup(&path);
    }
}
