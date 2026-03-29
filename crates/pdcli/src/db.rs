use chrono::DateTime;
use proton_drive_sdk::node::NodeUid;
use rusqlite::{params, Connection};

use crate::index::IndexEntry;

/// A minimal device record persisted to SQLite.
#[derive(Debug, Clone)]
pub struct DeviceCacheRow {
    pub device_id: String,
    pub name: String,
    pub root_uid: NodeUid,
    /// Raw integer: 1=Windows 2=macOS 3=Linux
    pub device_type_raw: u32,
    pub last_sync_time_rfc: Option<String>,
}

pub fn open_and_init(path: &std::path::Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS node_entries (
             uid               TEXT PRIMARY KEY,
             parent_uid        TEXT,
             name              TEXT NOT NULL,
             is_folder         INTEGER NOT NULL,
             size              INTEGER,
             modification_time TEXT,
             media_type        TEXT
         );
         CREATE TABLE IF NOT EXISTS indexed_folders (
             uid TEXT PRIMARY KEY
         );
         CREATE TABLE IF NOT EXISTS trash_cache (
             uid       TEXT PRIMARY KEY,
             name      TEXT NOT NULL,
             is_folder INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS devices_cache (
             device_id        TEXT PRIMARY KEY,
             name             TEXT NOT NULL,
             root_uid         TEXT NOT NULL,
             device_type_raw  INTEGER NOT NULL,
             last_sync_time   TEXT
         );",
    )?;
    Ok(conn)
}

pub fn save_entry(conn: &Connection, entry: &IndexEntry) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO node_entries
             (uid, parent_uid, name, is_folder, size, modification_time, media_type)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            entry.uid.to_string(),
            entry.parent_uid.as_ref().map(|u| u.to_string()),
            entry.name,
            entry.is_folder as i32,
            entry.size,
            entry.modification_time.map(|t| t.to_rfc3339()),
            entry.media_type,
        ],
    )?;
    Ok(())
}

pub fn delete_entry(conn: &Connection, uid: &NodeUid) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM node_entries WHERE uid = ?1",
        params![uid.to_string()],
    )?;
    Ok(())
}

pub fn mark_indexed(conn: &Connection, uid: &NodeUid) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO indexed_folders (uid) VALUES (?1)",
        params![uid.to_string()],
    )?;
    Ok(())
}

pub fn unmark_indexed(conn: &Connection, uid: &NodeUid) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM indexed_folders WHERE uid = ?1",
        params![uid.to_string()],
    )?;
    Ok(())
}

pub fn load_all_entries(conn: &Connection) -> anyhow::Result<Vec<IndexEntry>> {
    let mut stmt = conn.prepare(
        "SELECT uid, parent_uid, name, is_folder, size, modification_time, media_type
         FROM node_entries",
    )?;

    let entries = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i32>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(uid_s, parent_s, name, is_folder, size, mtime_s, media_type)| {
            let uid = NodeUid::try_parse(&uid_s)?;
            let parent_uid = parent_s.as_deref().and_then(NodeUid::try_parse);
            let modification_time = mtime_s
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));
            Some(IndexEntry {
                uid,
                parent_uid,
                name,
                is_folder: is_folder != 0,
                size,
                modification_time,
                media_type,
            })
        })
        .collect();

    Ok(entries)
}

pub fn load_indexed_folders(conn: &Connection) -> anyhow::Result<Vec<NodeUid>> {
    let mut stmt = conn.prepare("SELECT uid FROM indexed_folders")?;
    let uids = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .filter_map(|s| NodeUid::try_parse(&s))
        .collect();
    Ok(uids)
}

// ── Trash cache ───────────────────────────────────────────────────────────────

pub fn save_trash_cache(
    conn: &Connection,
    items: &[(NodeUid, String, bool)],
) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM trash_cache", [])?;
    for (uid, name, is_folder) in items {
        conn.execute(
            "INSERT INTO trash_cache (uid, name, is_folder) VALUES (?1, ?2, ?3)",
            params![uid.to_string(), name, *is_folder as i32],
        )?;
    }
    Ok(())
}

pub fn load_trash_cache(conn: &Connection) -> Vec<(NodeUid, String, bool)> {
    let Ok(mut stmt) =
        conn.prepare("SELECT uid, name, is_folder FROM trash_cache")
    else {
        return vec![];
    };
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i32>(2)?,
        ))
    })
    .map(|rows| {
        rows.filter_map(|r| r.ok())
            .filter_map(|(uid_s, name, is_folder)| {
                let uid = NodeUid::try_parse(&uid_s)?;
                Some((uid, name, is_folder != 0))
            })
            .collect()
    })
    .unwrap_or_default()
}

// ── Devices cache ─────────────────────────────────────────────────────────────

pub fn save_devices_cache(conn: &Connection, rows: &[DeviceCacheRow]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM devices_cache", [])?;
    for row in rows {
        conn.execute(
            "INSERT INTO devices_cache (device_id, name, root_uid, device_type_raw, last_sync_time)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.device_id,
                row.name,
                row.root_uid.to_string(),
                row.device_type_raw,
                row.last_sync_time_rfc,
            ],
        )?;
    }
    Ok(())
}

pub fn load_devices_cache(conn: &Connection) -> Vec<DeviceCacheRow> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT device_id, name, root_uid, device_type_raw, last_sync_time FROM devices_cache",
    ) else {
        return vec![];
    };
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, u32>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })
    .map(|rows| {
        rows.filter_map(|r| r.ok())
            .filter_map(|(device_id, name, root_uid_s, device_type_raw, last_sync_time_rfc)| {
                let root_uid = NodeUid::try_parse(&root_uid_s)?;
                Some(DeviceCacheRow { device_id, name, root_uid, device_type_raw, last_sync_time_rfc })
            })
            .collect()
    })
    .unwrap_or_default()
}
