use anyhow::Context;
use futures::stream::BoxStream;
use futures::StreamExt;
use proton_sdk_rs2::cache::CacheRepository;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

/// A [`CacheRepository`] backed by a SQLite database.
///
/// Entries are stored in a single `Entries` table keyed by a `TEXT` primary key.
/// An optional `Tags` table maps tags to their associated keys, enabling bulk
/// tag-based lookup and eviction.
///
/// When `max_cache_size` is set the repository enforces an LRU eviction policy:
/// before inserting a new key it checks that the total number of entries does
/// not exceed the limit. If the limit is reached it evicts the 25 % of entries
/// that were least recently accessed (at least one entry is always evicted).
pub struct SqliteCacheRepository {
    connection: Mutex<Connection>,
    max_cache_size: Option<usize>,
}

impl SqliteCacheRepository {
    /// Opens an in-memory SQLite cache.
    ///
    /// The database lives for the lifetime of this repository and is destroyed
    /// when the repository is dropped.
    ///
    /// # Arguments
    ///
    /// * `max_cache_size` – Maximum number of entries before LRU eviction
    ///   kicks in.  Pass `None` to disable the size cap.
    pub fn open_in_memory(max_cache_size: Option<usize>) -> anyhow::Result<Self> {
        let connection = Connection::open_in_memory().context("Failed to open in-memory SQLite")?;
        Self::init(connection, max_cache_size)
    }

    /// Opens a file-backed SQLite cache at the given path.
    ///
    /// The file is created if it does not already exist.
    ///
    /// # Arguments
    ///
    /// * `path` – Filesystem path for the SQLite database file.
    /// * `max_cache_size` – Maximum number of entries before LRU eviction
    ///   kicks in.  Pass `None` to disable the size cap.
    pub fn open_file(path: impl AsRef<Path>, max_cache_size: Option<usize>) -> anyhow::Result<Self> {
        let connection = Connection::open(path).context("Failed to open SQLite file")?;
        Self::init(connection, max_cache_size)
    }

    fn init(connection: Connection, max_cache_size: Option<usize>) -> anyhow::Result<Self> {
        if let Some(size) = max_cache_size {
            anyhow::ensure!(size > 0, "max_cache_size must be greater than 0");
        }
        initialize_schema(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
            max_cache_size,
        })
    }
}

#[async_trait::async_trait]
impl CacheRepository for SqliteCacheRepository {
    async fn set(&self, key: &str, value: String, tags: Vec<String>) -> anyhow::Result<()> {
        let conn = self.connection.lock().unwrap();
        let max = self.max_cache_size;

        if let Some(max_size) = max {
            let count: usize = conn
                .query_row("SELECT COUNT(*) FROM Entries", [], |row| row.get(0))
                .context("Failed to count entries")?;

            if count >= max_size {
                let key_exists: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM Entries WHERE Key = ?1)",
                        params![key],
                        |row| row.get(0),
                    )
                    .context("Failed to check key existence")?;

                if !key_exists {
                    let evict_count = std::cmp::max(1, max_size / 4);
                    conn.execute(
                        "DELETE FROM Entries WHERE Key IN \
                         (SELECT Key FROM Entries ORDER BY LastAccessedUtc ASC LIMIT ?1)",
                        params![evict_count as i64],
                    )
                    .context("Failed to evict LRU entries")?;
                }
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        conn.execute(
            "INSERT INTO Entries (Key, Value, LastAccessedUtc) VALUES (?1, ?2, ?3) \
             ON CONFLICT(Key) DO UPDATE SET Value = excluded.Value, LastAccessedUtc = excluded.LastAccessedUtc",
            params![key, value, now],
        )
        .context("Failed to upsert cache entry")?;

        conn.execute("DELETE FROM Tags WHERE Key = ?1", params![key])
            .context("Failed to clear old tags")?;

        for tag in &tags {
            conn.execute(
                "INSERT OR IGNORE INTO Tags (Tag, Key) VALUES (?1, ?2)",
                params![tag, key],
            )
            .context("Failed to insert tag")?;
        }

        Ok(())
    }

    async fn remove(&self, key: &str) -> anyhow::Result<()> {
        let conn = self.connection.lock().unwrap();
        conn.execute("DELETE FROM Entries WHERE Key = ?1", params![key])
            .context("Failed to remove cache entry")?;
        Ok(())
    }

    async fn remove_by_tag(&self, tag: &str) -> anyhow::Result<()> {
        let conn = self.connection.lock().unwrap();
        conn.execute(
            "DELETE FROM Entries WHERE Key IN (SELECT Key FROM Tags WHERE Tag = ?1)",
            params![tag],
        )
        .context("Failed to remove entries by tag")?;
        Ok(())
    }

    async fn clear(&self) -> anyhow::Result<()> {
        let conn = self.connection.lock().unwrap();
        conn.execute("DELETE FROM Entries", [])
            .context("Failed to clear cache")?;
        Ok(())
    }

    async fn try_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.connection.lock().unwrap();

        let result: Option<String> = conn
            .query_row(
                "SELECT Value FROM Entries WHERE Key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .context("Failed to query cache entry")?;

        if result.is_some() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            conn.execute(
                "UPDATE Entries SET LastAccessedUtc = ?1 WHERE Key = ?2",
                params![now, key],
            )
            .context("Failed to update last accessed timestamp")?;
        }

        Ok(result)
    }

    fn get_by_tags(&self, tags: Vec<String>) -> BoxStream<'_, anyhow::Result<(String, String)>> {
        let conn = self.connection.lock().unwrap();

        if tags.is_empty() {
            return futures::stream::empty().boxed();
        }

        let placeholders: String = tags
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "SELECT e.Key, e.Value FROM Entries e \
             WHERE e.Key IN (SELECT t.Key FROM Tags t WHERE t.Tag IN ({placeholders}) \
             GROUP BY t.Key HAVING COUNT(DISTINCT t.Tag) = {count})",
            placeholders = placeholders,
            count = tags.len(),
        );

        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                return futures::stream::once(async move {
                    Err(anyhow::anyhow!("Failed to prepare query: {}", e))
                })
                .boxed()
            }
        };

        let params_refs: Vec<&dyn rusqlite::ToSql> = tags
            .iter()
            .map(|t| t as &dyn rusqlite::ToSql)
            .collect();

        let rows_result: Result<Vec<(String, String)>, _> = stmt
            .query_map(params_refs.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .and_then(|mapped| mapped.collect());

        let results = match rows_result {
            Ok(v) => v,
            Err(e) => {
                return futures::stream::once(async move {
                    Err(anyhow::anyhow!("Failed to fetch entries by tags: {}", e))
                })
                .boxed()
            }
        };

        futures::stream::iter(results.into_iter().map(Ok)).boxed()
    }
}

fn initialize_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;

         CREATE TABLE IF NOT EXISTS Entries (
             Key             TEXT    NOT NULL,
             Value           TEXT    NOT NULL,
             LastAccessedUtc INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (Key)
         );

         CREATE INDEX IF NOT EXISTS idx_entries_last_accessed ON Entries(LastAccessedUtc);

         CREATE TABLE IF NOT EXISTS Tags (
             Tag TEXT NOT NULL,
             Key TEXT NOT NULL,
             PRIMARY KEY (Tag, Key),
             FOREIGN KEY (Key) REFERENCES Entries(Key) ON DELETE CASCADE
         );",
    )
    .context("Failed to initialize SQLite schema")
}
