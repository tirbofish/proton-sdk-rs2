//! Local record of photos already in the Photos library, so repeat imports can skip
//! known content without scanning the whole remote timeline.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Entries keyed by the account-keyed content fingerprint, so a cache shared between
/// accounts never matches another account's photos.
pub struct UploadedPhotoCache {
    conn: Connection,
    ttl: Duration,
}

/// A local file identified by where it is and how it looks on disk, without reading it.
pub struct LocalFile {
    pub path: PathBuf,
    pub size: i64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub modified: i64,
}

impl LocalFile {
    pub fn new(path: &Path, metadata: &std::fs::Metadata) -> Option<Self> {
        let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Self {
            path: path.to_owned(),
            size: i64::try_from(metadata.len()).ok()?,
            modified: i64::try_from(modified.as_nanos()).ok()?,
        })
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl UploadedPhotoCache {
    pub fn default_path() -> anyhow::Result<PathBuf> {
        platform_dirs::AppDirs::new(Some("pdcli"), false)
            .map(|dirs| dirs.cache_dir.join("uploaded-photos.db"))
            .ok_or_else(|| anyhow::anyhow!("failed to resolve cache directory"))
    }

    pub fn open_default(ttl: Duration) -> anyhow::Result<Self> {
        let path = Self::default_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::open_with_key(&path, &crate::credentials::cache_master_key()?, ttl)
    }

    pub fn open_with_key(path: &Path, key: &[u8], ttl: Duration) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        crate::db::apply_sqlcipher_key(&conn, key)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS uploaded_photos (
                fingerprint BLOB PRIMARY KEY,
                uid TEXT NOT NULL,
                seen_at INTEGER NOT NULL
            );
            -- Local files whose content is known, so unchanged files skip hashing.
            CREATE TABLE IF NOT EXISTS imported_files (
                volume TEXT NOT NULL,
                path BLOB NOT NULL,
                size INTEGER NOT NULL,
                modified INTEGER NOT NULL,
                fingerprint BLOB NOT NULL,
                PRIMARY KEY (volume, path)
            );
            ",
        )?;
        crate::credentials::restrict_permissions(path, 0o600);
        let cache = Self { conn, ttl };
        cache.conn.execute(
            "DELETE FROM uploaded_photos WHERE seen_at < ?1",
            [cache.oldest_fresh()],
        )?;
        cache.conn.execute(
            "DELETE FROM imported_files
             WHERE fingerprint NOT IN (SELECT fingerprint FROM uploaded_photos)",
            [],
        )?;
        Ok(cache)
    }

    fn oldest_fresh(&self) -> i64 {
        now_unix().saturating_sub(i64::try_from(self.ttl.as_secs()).unwrap_or(i64::MAX))
    }

    /// Returns the UID recorded for this content if it was seen within the TTL.
    pub fn get(&self, fingerprint: &[u8]) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT uid FROM uploaded_photos WHERE fingerprint = ?1 AND seen_at >= ?2",
                params![fingerprint, self.oldest_fresh()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Records content as present in the library, restarting its TTL.
    pub fn insert(&self, fingerprint: &[u8], uid: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO uploaded_photos (fingerprint, uid, seen_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(fingerprint) DO UPDATE SET uid = excluded.uid, seen_at = excluded.seen_at",
            params![fingerprint, uid, now_unix()],
        )?;
        Ok(())
    }

    /// Returns the library UID for a local file unchanged since it was last imported.
    pub fn get_file(&self, volume: &str, file: &LocalFile) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT u.uid FROM imported_files f
                 JOIN uploaded_photos u ON u.fingerprint = f.fingerprint
                 WHERE f.volume = ?1 AND f.path = ?2 AND f.size = ?3 AND f.modified = ?4
                   AND u.seen_at >= ?5",
                params![
                    volume,
                    file.path.as_os_str().as_encoded_bytes(),
                    file.size,
                    file.modified,
                    self.oldest_fresh()
                ],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Records which content a local file holds, alongside the library UID for it.
    pub fn insert_file(
        &self,
        volume: &str,
        file: &LocalFile,
        fingerprint: &[u8],
        uid: &str,
    ) -> anyhow::Result<()> {
        self.insert(fingerprint, uid)?;
        self.conn.execute(
            "INSERT INTO imported_files (volume, path, size, modified, fingerprint)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(volume, path) DO UPDATE SET
                size = excluded.size, modified = excluded.modified,
                fingerprint = excluded.fingerprint",
            params![
                volume,
                file.path.as_os_str().as_encoded_bytes(),
                file.size,
                file.modified,
                fingerprint
            ],
        )?;
        Ok(())
    }

    #[cfg(test)]
    fn set_seen_at(&self, fingerprint: &[u8], seen_at: i64) {
        self.conn
            .execute(
                "UPDATE uploaded_photos SET seen_at = ?2 WHERE fingerprint = ?1",
                params![fingerprint, seen_at],
            )
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploaded_photo_cache_hits_until_ttl_expires_and_survives_reopen() {
        let path =
            std::env::temp_dir().join(format!("pdcli-photo-cache-{}.db", rand::random::<u64>()));
        let key = [7u8; 32];
        let ttl = Duration::from_secs(60);
        let cache = UploadedPhotoCache::open_with_key(&path, &key, ttl).unwrap();
        assert_eq!(cache.get(b"fresh").unwrap(), None);
        cache.insert(b"fresh", "vol~one").unwrap();
        cache.insert(b"stale", "vol~two").unwrap();
        cache.set_seen_at(b"stale", now_unix() - 120);
        assert_eq!(cache.get(b"fresh").unwrap().as_deref(), Some("vol~one"));
        assert_eq!(cache.get(b"stale").unwrap(), None);
        drop(cache);

        let cache = UploadedPhotoCache::open_with_key(&path, &key, ttl).unwrap();
        assert_eq!(cache.get(b"fresh").unwrap().as_deref(), Some("vol~one"));
        drop(cache);
        assert!(UploadedPhotoCache::open_with_key(&path, &[8u8; 32], ttl).is_err());
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn unchanged_files_are_known_per_volume_until_they_change_or_expire() {
        let path =
            std::env::temp_dir().join(format!("pdcli-photo-files-{}.db", rand::random::<u64>()));
        let cache =
            UploadedPhotoCache::open_with_key(&path, &[7u8; 32], Duration::from_secs(60)).unwrap();
        let file = LocalFile {
            path: PathBuf::from("/photos/a.jpg"),
            size: 10,
            modified: 1,
        };
        cache
            .insert_file("vol", &file, b"content", "vol~a")
            .unwrap();
        assert_eq!(
            cache.get_file("vol", &file).unwrap().as_deref(),
            Some("vol~a")
        );
        assert_eq!(cache.get_file("other", &file).unwrap(), None);
        let edited = LocalFile {
            modified: 2,
            ..file
        };
        assert_eq!(cache.get_file("vol", &edited).unwrap(), None);
        cache.set_seen_at(b"content", now_unix() - 120);
        let file = LocalFile {
            modified: 1,
            ..edited
        };
        assert_eq!(cache.get_file("vol", &file).unwrap(), None);
        drop(cache);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
