//! Offline index for Proton Drive.
//!
//! The index is the primary data store for the FUSE filesystem. It stores:
//! - Node metadata (files and folders)
//! - Offline file tracking (which files are hydrated)
//! - Pending mutations (writes/deletes queued for sync)
//!
//! The FUSE layer reads from the index first, falling back to network only
//! when necessary. All network responses update the index.
//!
//! ## Event System
//! The index emits events via a tokio broadcast channel when nodes change.
//! FUSE subscribes to these events to invalidate/refresh its internal state.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Event buffer size for the broadcast channel.
const EVENT_BUFFER_SIZE: usize = 256;

/// Events emitted by the index when data changes.
#[derive(Debug, Clone)]
pub enum IndexEvent {
    /// A node was added or updated.
    NodeUpserted {
        link_id: String,
        parent_link_id: Option<String>,
        node_type: NodeType,
    },
    /// A node was removed.
    NodeRemoved {
        link_id: String,
        parent_link_id: Option<String>,
    },
    /// Multiple children were loaded for a folder.
    ChildrenLoaded {
        parent_link_id: String,
        count: usize,
    },
    /// A file's offline status changed.
    OfflineStatusChanged {
        link_id: String,
        available: bool,
    },
    /// A mutation was added to the queue.
    MutationQueued {
        mutation_type: String,
    },
    /// A mutation was removed (synced or cancelled).
    MutationRemoved {
        id: i64,
    },
    /// Index was cleared/reset.
    IndexCleared,
}

/// The offline index database.
pub struct OfflineIndex {
    connection: Mutex<Connection>,
    index_path: PathBuf,
    event_tx: broadcast::Sender<IndexEvent>,
}

/// Node type stored in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum NodeType {
    File = 0,
    Folder = 1,
}

impl NodeType {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => NodeType::File,
            _ => NodeType::Folder,
        }
    }
}

/// A cached node in the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedNode {
    /// Link ID (primary key)
    pub link_id: String,
    /// Parent link ID (null for root)
    pub parent_link_id: Option<String>,
    /// Volume ID
    pub volume_id: String,
    /// Decrypted name
    pub name: String,
    /// Node type (file or folder)
    pub node_type: NodeType,
    /// MIME type (for files)
    pub mime_type: Option<String>,
    /// File size in bytes (claimed plaintext size)
    pub size: Option<i64>,
    /// Revision UID (for files, used to check if content is stale)
    pub revision_id: Option<String>,
    /// Creation time
    pub creation_time: DateTime<Utc>,
    /// Modification time (for files)
    pub modification_time: Option<DateTime<Utc>>,
    /// When this node was last fetched from the server
    pub fetched_at: DateTime<Utc>,
    /// Whether this node exists only locally (not yet synced)
    pub local_only: bool,
    /// Whether this node has been deleted locally (pending sync)
    pub pending_delete: bool,
}

/// Status of offline availability for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineStatus {
    /// Not available offline
    NotAvailable,
    /// Content is cached and available
    Available,
    /// Content is stale (revision changed)
    Stale,
}

/// A pending mutation to be synced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PendingMutation {
    /// Create a new file
    CreateFile {
        link_id: String,
        parent_link_id: String,
        name: String,
        mime_type: String,
        content_path: String,
    },
    /// Update file content
    UpdateFile {
        link_id: String,
        revision_id: String,
        content_path: String,
    },
    /// Rename a node
    Rename {
        link_id: String,
        new_parent_link_id: Option<String>,
        new_name: String,
    },
    /// Delete a node
    Delete {
        link_id: String,
    },
    /// Create a folder
    CreateFolder {
        link_id: String,
        parent_link_id: String,
        name: String,
    },
}

/// Serialized mutation for storage.
#[derive(Debug, Clone)]
pub struct StoredMutation {
    pub id: i64,
    pub mutation: PendingMutation,
    pub created_at: DateTime<Utc>,
    pub retry_count: u32,
}

impl OfflineIndex {
    /// Open or create an index at the given path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create index directory")?;
        }

        let connection = Connection::open(path)
            .context("Failed to open index database")?;

        Self::init_schema(&connection)?;

        let (event_tx, _) = broadcast::channel(EVENT_BUFFER_SIZE);

        Ok(Self {
            connection: Mutex::new(connection),
            index_path: path.to_path_buf(),
            event_tx,
        })
    }

    /// Open an in-memory index (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()
            .context("Failed to open in-memory index")?;

        Self::init_schema(&connection)?;

        let (event_tx, _) = broadcast::channel(EVENT_BUFFER_SIZE);

        Ok(Self {
            connection: Mutex::new(connection),
            index_path: PathBuf::new(),
            event_tx,
        })
    }

    /// Subscribe to index events.
    /// 
    /// Returns a receiver that will receive events when the index changes.
    /// FUSE should call this to get notified of changes from hydrate, sync, etc.
    pub fn subscribe(&self) -> broadcast::Receiver<IndexEvent> {
        self.event_tx.subscribe()
    }

    /// Emit an event (ignores errors if no subscribers).
    fn emit(&self, event: IndexEvent) {
        // Ignore send errors - they just mean no one is listening
        let _ = self.event_tx.send(event);
    }

    /// Initialize the database schema.
    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            -- Node metadata cache
            CREATE TABLE IF NOT EXISTS nodes (
                link_id TEXT PRIMARY KEY,
                parent_link_id TEXT,
                volume_id TEXT NOT NULL,
                
                name TEXT NOT NULL,
                node_type INTEGER NOT NULL,
                mime_type TEXT,
                size INTEGER,
                revision_id TEXT,
                creation_time INTEGER NOT NULL,
                modification_time INTEGER,
                fetched_at INTEGER NOT NULL,
                local_only INTEGER NOT NULL DEFAULT 0,
                pending_delete INTEGER NOT NULL DEFAULT 0
            );

            -- Index for parent lookups (listing children)
            CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(parent_link_id);

            -- Offline files tracking
            CREATE TABLE IF NOT EXISTS offline_files (
                link_id TEXT PRIMARY KEY,
                revision_id TEXT NOT NULL,
                content_path TEXT NOT NULL,
                size INTEGER NOT NULL,
                cached_at INTEGER NOT NULL,
                FOREIGN KEY (link_id) REFERENCES nodes(link_id) ON DELETE CASCADE
            );

            -- Pending mutations queue
            CREATE TABLE IF NOT EXISTS mutations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mutation_type TEXT NOT NULL,
                mutation_data TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                retry_count INTEGER NOT NULL DEFAULT 0
            );

            -- Metadata for sync state
            CREATE TABLE IF NOT EXISTS sync_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )
        .context("Failed to initialize index schema")?;

        Ok(())
    }

    /// Get the index file path.
    pub fn path(&self) -> &Path {
        &self.index_path
    }

    // =========================================================================
    // Node operations
    // =========================================================================

    /// Insert or update a node in the index.
    pub fn upsert_node(&self, node: &IndexedNode) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.execute(
            r#"
            INSERT INTO nodes (
                link_id, parent_link_id, volume_id, name, node_type,
                mime_type, size, revision_id, creation_time, modification_time,
                fetched_at, local_only, pending_delete
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(link_id) DO UPDATE SET
                parent_link_id = excluded.parent_link_id,
                name = excluded.name,
                mime_type = excluded.mime_type,
                size = excluded.size,
                revision_id = excluded.revision_id,
                modification_time = excluded.modification_time,
                fetched_at = excluded.fetched_at,
                local_only = excluded.local_only,
                pending_delete = excluded.pending_delete
            "#,
            params![
                node.link_id,
                node.parent_link_id,
                node.volume_id,
                node.name,
                node.node_type as u8,
                node.mime_type,
                node.size,
                node.revision_id,
                node.creation_time.timestamp(),
                node.modification_time.map(|t| t.timestamp()),
                node.fetched_at.timestamp(),
                node.local_only as i32,
                node.pending_delete as i32,
            ],
        )
        .context("Failed to upsert node")?;

        // Emit event
        self.emit(IndexEvent::NodeUpserted {
            link_id: node.link_id.clone(),
            parent_link_id: node.parent_link_id.clone(),
            node_type: node.node_type,
        });

        Ok(())
    }

    /// Get a node by link ID.
    pub fn get_node(&self, link_id: &str) -> Result<Option<IndexedNode>> {
        let conn = self.connection.lock().unwrap();

        conn.query_row(
            r#"
            SELECT link_id, parent_link_id, volume_id, name, node_type,
                   mime_type, size, revision_id, creation_time, modification_time,
                   fetched_at, local_only, pending_delete
            FROM nodes WHERE link_id = ?1
            "#,
            params![link_id],
            |row| {
                Ok(IndexedNode {
                    link_id: row.get(0)?,
                    parent_link_id: row.get(1)?,
                    volume_id: row.get(2)?,
                    name: row.get(3)?,
                    node_type: NodeType::from_u8(row.get(4)?),
                    mime_type: row.get(5)?,
                    size: row.get(6)?,
                    revision_id: row.get(7)?,
                    creation_time: DateTime::from_timestamp(row.get(8)?, 0)
                        .unwrap_or_else(|| Utc::now()),
                    modification_time: row.get::<_, Option<i64>>(9)?
                        .and_then(|t| DateTime::from_timestamp(t, 0)),
                    fetched_at: DateTime::from_timestamp(row.get(10)?, 0)
                        .unwrap_or_else(|| Utc::now()),
                    local_only: row.get::<_, i32>(11)? != 0,
                    pending_delete: row.get::<_, i32>(12)? != 0,
                })
            },
        )
        .optional()
        .context("Failed to get node")
    }

    /// Get all children of a parent node.
    pub fn get_children(&self, parent_link_id: &str) -> Result<Vec<IndexedNode>> {
        let conn = self.connection.lock().unwrap();

        let mut stmt = conn.prepare(
            r#"
            SELECT link_id, parent_link_id, volume_id, name, node_type,
                   mime_type, size, revision_id, creation_time, modification_time,
                   fetched_at, local_only, pending_delete
            FROM nodes 
            WHERE parent_link_id = ?1 AND pending_delete = 0
            ORDER BY node_type DESC, name ASC
            "#,
        )?;

        let nodes = stmt
            .query_map(params![parent_link_id], |row| {
                Ok(IndexedNode {
                    link_id: row.get(0)?,
                    parent_link_id: row.get(1)?,
                    volume_id: row.get(2)?,
                    name: row.get(3)?,
                    node_type: NodeType::from_u8(row.get(4)?),
                    mime_type: row.get(5)?,
                    size: row.get(6)?,
                    revision_id: row.get(7)?,
                    creation_time: DateTime::from_timestamp(row.get(8)?, 0)
                        .unwrap_or_else(|| Utc::now()),
                    modification_time: row.get::<_, Option<i64>>(9)?
                        .and_then(|t| DateTime::from_timestamp(t, 0)),
                    fetched_at: DateTime::from_timestamp(row.get(10)?, 0)
                        .unwrap_or_else(|| Utc::now()),
                    local_only: row.get::<_, i32>(11)? != 0,
                    pending_delete: row.get::<_, i32>(12)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to get children")?;

        Ok(nodes)
    }

    /// Batch upsert multiple children and emit a single event.
    /// More efficient than calling upsert_node repeatedly.
    pub fn upsert_children(&self, parent_link_id: &str, children: &[IndexedNode]) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        for node in children {
            conn.execute(
                r#"
                INSERT INTO nodes (
                    link_id, parent_link_id, volume_id, name, node_type,
                    mime_type, size, revision_id, creation_time, modification_time,
                    fetched_at, local_only, pending_delete
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ON CONFLICT(link_id) DO UPDATE SET
                    parent_link_id = excluded.parent_link_id,
                    name = excluded.name,
                    mime_type = excluded.mime_type,
                    size = excluded.size,
                    revision_id = excluded.revision_id,
                    modification_time = excluded.modification_time,
                    fetched_at = excluded.fetched_at,
                    local_only = excluded.local_only,
                    pending_delete = excluded.pending_delete
                "#,
                params![
                    node.link_id,
                    node.parent_link_id,
                    node.volume_id,
                    node.name,
                    node.node_type as u8,
                    node.mime_type,
                    node.size,
                    node.revision_id,
                    node.creation_time.timestamp(),
                    node.modification_time.map(|t| t.timestamp()),
                    node.fetched_at.timestamp(),
                    node.local_only as i32,
                    node.pending_delete as i32,
                ],
            )
            .context("Failed to upsert child node")?;
        }

        drop(conn);

        // Emit single event for batch operation
        self.emit(IndexEvent::ChildrenLoaded {
            parent_link_id: parent_link_id.to_string(),
            count: children.len(),
        });

        Ok(())
    }

    /// Check if we have cached children for a parent.
    pub fn has_children(&self, parent_link_id: &str) -> Result<bool> {
        let conn = self.connection.lock().unwrap();

        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE parent_link_id = ?1",
            params![parent_link_id],
            |row| row.get(0),
        )?;

        Ok(count > 0)
    }

    /// Delete a node from the index.
    pub fn delete_node(&self, link_id: &str) -> Result<()> {
        // Get parent before deleting
        let parent_link_id = {
            let conn = self.connection.lock().unwrap();
            conn.query_row(
                "SELECT parent_link_id FROM nodes WHERE link_id = ?1",
                params![link_id],
                |row| row.get::<_, Option<String>>(0),
            ).ok().flatten()
        };

        let conn = self.connection.lock().unwrap();
        conn.execute("DELETE FROM nodes WHERE link_id = ?1", params![link_id])
            .context("Failed to delete node")?;
        drop(conn);

        // Emit event
        self.emit(IndexEvent::NodeRemoved {
            link_id: link_id.to_string(),
            parent_link_id,
        });

        Ok(())
    }

    /// Delete a node and all its descendants from the index (recursive).
    /// Use this when deleting folders to ensure children are also removed.
    pub fn delete_node_recursive(&self, link_id: &str) -> Result<usize> {
        // Get parent before deleting (for event)
        let parent_link_id = {
            let conn = self.connection.lock().unwrap();
            conn.query_row(
                "SELECT parent_link_id FROM nodes WHERE link_id = ?1",
                params![link_id],
                |row| row.get::<_, Option<String>>(0),
            ).ok().flatten()
        };

        // First, collect all descendant link_ids using recursive CTE
        let descendant_ids: Vec<String> = {
            let conn = self.connection.lock().unwrap();
            let mut stmt = conn.prepare(
                r#"
                WITH RECURSIVE descendants(link_id) AS (
                    SELECT link_id FROM nodes WHERE link_id = ?1
                    UNION ALL
                    SELECT n.link_id FROM nodes n
                    INNER JOIN descendants d ON n.parent_link_id = d.link_id
                )
                SELECT link_id FROM descendants
                "#,
            )?;
            stmt.query_map(params![link_id], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to collect descendants")?
        };

        let count = descendant_ids.len();
        tracing::debug!("Deleting {} nodes (recursive from {})", count, link_id);

        // Delete all descendants and the node itself
        {
            let conn = self.connection.lock().unwrap();
            // Delete offline files for all descendants
            for id in &descendant_ids {
                let _ = conn.execute("DELETE FROM offline_files WHERE link_id = ?1", params![id]);
            }
            // Delete nodes - use the recursive CTE again
            conn.execute(
                r#"
                WITH RECURSIVE descendants(link_id) AS (
                    SELECT link_id FROM nodes WHERE link_id = ?1
                    UNION ALL
                    SELECT n.link_id FROM nodes n
                    INNER JOIN descendants d ON n.parent_link_id = d.link_id
                )
                DELETE FROM nodes WHERE link_id IN (SELECT link_id FROM descendants)
                "#,
                params![link_id],
            ).context("Failed to delete node tree")?;
        }

        // Emit event for the root node (FUSE handler will clean up children)
        self.emit(IndexEvent::NodeRemoved {
            link_id: link_id.to_string(),
            parent_link_id,
        });

        Ok(count)
    }

    /// Mark a node as pending delete (for offline deletion).
    pub fn mark_pending_delete(&self, link_id: &str) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.execute(
            "UPDATE nodes SET pending_delete = 1 WHERE link_id = ?1",
            params![link_id],
        )
        .context("Failed to mark node as pending delete")?;

        Ok(())
    }

    /// Get a node by name within a parent.
    pub fn get_child_by_name(&self, parent_link_id: &str, name: &str) -> Result<Option<IndexedNode>> {
        let conn = self.connection.lock().unwrap();

        conn.query_row(
            r#"
            SELECT link_id, parent_link_id, volume_id, name, node_type,
                   mime_type, size, revision_id, creation_time, modification_time,
                   fetched_at, local_only, pending_delete
            FROM nodes 
            WHERE parent_link_id = ?1 AND name = ?2 AND pending_delete = 0
            "#,
            params![parent_link_id, name],
            |row| {
                Ok(IndexedNode {
                    link_id: row.get(0)?,
                    parent_link_id: row.get(1)?,
                    volume_id: row.get(2)?,
                    name: row.get(3)?,
                    node_type: NodeType::from_u8(row.get(4)?),
                    mime_type: row.get(5)?,
                    size: row.get(6)?,
                    revision_id: row.get(7)?,
                    creation_time: DateTime::from_timestamp(row.get(8)?, 0)
                        .unwrap_or_else(|| Utc::now()),
                    modification_time: row.get::<_, Option<i64>>(9)?
                        .and_then(|t| DateTime::from_timestamp(t, 0)),
                    fetched_at: DateTime::from_timestamp(row.get(10)?, 0)
                        .unwrap_or_else(|| Utc::now()),
                    local_only: row.get::<_, i32>(11)? != 0,
                    pending_delete: row.get::<_, i32>(12)? != 0,
                })
            },
        )
        .optional()
        .context("Failed to get child by name")
    }

    // =========================================================================
    // Offline file tracking
    // =========================================================================

    /// Mark a file as available offline.
    pub fn mark_offline(&self, link_id: &str, revision_id: &str, content_path: &str, size: i64) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.execute(
            r#"
            INSERT INTO offline_files (link_id, revision_id, content_path, size, cached_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(link_id) DO UPDATE SET
                revision_id = excluded.revision_id,
                content_path = excluded.content_path,
                size = excluded.size,
                cached_at = excluded.cached_at
            "#,
            params![
                link_id,
                revision_id,
                content_path,
                size,
                Utc::now().timestamp(),
            ],
        )
        .context("Failed to mark file as offline")?;
        drop(conn);

        // Emit event
        self.emit(IndexEvent::OfflineStatusChanged {
            link_id: link_id.to_string(),
            available: true,
        });

        Ok(())
    }

    /// Check if a file is available offline.
    pub fn get_offline_status(&self, link_id: &str) -> Result<OfflineStatus> {
        let conn = self.connection.lock().unwrap();

        // Get the current node's revision
        let node_revision: Option<String> = conn
            .query_row(
                "SELECT revision_id FROM nodes WHERE link_id = ?1",
                params![link_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();

        // Get the cached revision
        let cached_revision: Option<String> = conn
            .query_row(
                "SELECT revision_id FROM offline_files WHERE link_id = ?1",
                params![link_id],
                |row| row.get(0),
            )
            .optional()?;

        match (node_revision, cached_revision) {
            (Some(node_rev), Some(cached_rev)) => {
                if node_rev == cached_rev {
                    Ok(OfflineStatus::Available)
                } else {
                    Ok(OfflineStatus::Stale)
                }
            }
            (_, None) => Ok(OfflineStatus::NotAvailable),
            (None, Some(_)) => Ok(OfflineStatus::Available), // Node might not be in index yet
        }
    }

    /// Get the content path for an offline file.
    pub fn get_offline_content_path(&self, link_id: &str) -> Result<Option<String>> {
        let conn = self.connection.lock().unwrap();

        conn.query_row(
            "SELECT content_path FROM offline_files WHERE link_id = ?1",
            params![link_id],
            |row| row.get(0),
        )
        .optional()
        .context("Failed to get offline content path")
    }

    /// Remove offline status for a file.
    pub fn remove_offline(&self, link_id: &str) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.execute(
            "DELETE FROM offline_files WHERE link_id = ?1",
            params![link_id],
        )
        .context("Failed to remove offline status")?;
        drop(conn);

        // Emit event
        self.emit(IndexEvent::OfflineStatusChanged {
            link_id: link_id.to_string(),
            available: false,
        });

        Ok(())
    }

    /// Get total size of offline files.
    pub fn get_offline_size(&self) -> Result<i64> {
        let conn = self.connection.lock().unwrap();

        let size: i64 = conn.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM offline_files",
            [],
            |row| row.get(0),
        )?;

        Ok(size)
    }

    /// Get all offline files.
    pub fn get_all_offline_files(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.connection.lock().unwrap();

        let mut stmt = conn.prepare(
            "SELECT link_id, content_path, size FROM offline_files ORDER BY cached_at DESC",
        )?;

        let files = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to get offline files")?;

        Ok(files)
    }

    // =========================================================================
    // Mutation queue
    // =========================================================================

    /// Queue a mutation for later sync.
    pub fn queue_mutation(&self, mutation: &PendingMutation) -> Result<i64> {
        let conn = self.connection.lock().unwrap();

        let mutation_type = match mutation {
            PendingMutation::CreateFile { .. } => "create_file",
            PendingMutation::UpdateFile { .. } => "update_file",
            PendingMutation::Rename { .. } => "rename",
            PendingMutation::Delete { .. } => "delete",
            PendingMutation::CreateFolder { .. } => "create_folder",
        };

        let mutation_data =
            serde_json::to_string(mutation).context("Failed to serialize mutation")?;

        conn.execute(
            r#"
            INSERT INTO mutations (mutation_type, mutation_data, created_at, retry_count)
            VALUES (?1, ?2, ?3, 0)
            "#,
            params![mutation_type, mutation_data, Utc::now().timestamp()],
        )
        .context("Failed to queue mutation")?;

        let id = conn.last_insert_rowid();
        drop(conn);

        // Emit event
        self.emit(IndexEvent::MutationQueued {
            mutation_type: mutation_type.to_string(),
        });

        Ok(id)
    }

    /// Get all pending mutations.
    pub fn get_pending_mutations(&self) -> Result<Vec<StoredMutation>> {
        let conn = self.connection.lock().unwrap();

        let mut stmt = conn.prepare(
            "SELECT id, mutation_data, created_at, retry_count FROM mutations ORDER BY id ASC",
        )?;

        let mutations = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let data: String = row.get(1)?;
                let created_at: i64 = row.get(2)?;
                let retry_count: u32 = row.get(3)?;

                Ok((id, data, created_at, retry_count))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|(id, data, created_at, retry_count)| {
                let mutation: PendingMutation = serde_json::from_str(&data).ok()?;
                Some(StoredMutation {
                    id,
                    mutation,
                    created_at: DateTime::from_timestamp(created_at, 0)?,
                    retry_count,
                })
            })
            .collect();

        Ok(mutations)
    }

    /// Remove a mutation from the queue.
    pub fn remove_mutation(&self, id: i64) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.execute("DELETE FROM mutations WHERE id = ?1", params![id])
            .context("Failed to remove mutation")?;
        drop(conn);

        // Emit event
        self.emit(IndexEvent::MutationRemoved { id });

        Ok(())
    }

    /// Increment retry count for a mutation.
    pub fn increment_mutation_retry(&self, id: i64) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.execute(
            "UPDATE mutations SET retry_count = retry_count + 1 WHERE id = ?1",
            params![id],
        )
        .context("Failed to increment retry count")?;

        Ok(())
    }

    /// Get the number of pending mutations.
    pub fn pending_mutation_count(&self) -> Result<i64> {
        let conn = self.connection.lock().unwrap();

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM mutations", [], |row| row.get(0))?;

        Ok(count)
    }

    // =========================================================================
    // Sync state
    // =========================================================================

    /// Get a sync state value.
    pub fn get_sync_state(&self, key: &str) -> Result<Option<String>> {
        let conn = self.connection.lock().unwrap();

        conn.query_row(
            "SELECT value FROM sync_state WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .context("Failed to get sync state")
    }

    /// Set a sync state value.
    pub fn set_sync_state(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.execute(
            r#"
            INSERT INTO sync_state (key, value) VALUES (?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
            params![key, value],
        )
        .context("Failed to set sync state")?;

        Ok(())
    }

    /// Clear all index data (but keep schema).
    pub fn clear(&self) -> Result<()> {
        let conn = self.connection.lock().unwrap();

        conn.execute_batch(
            r#"
            DELETE FROM mutations;
            DELETE FROM offline_files;
            DELETE FROM nodes;
            DELETE FROM sync_state;
            "#,
        )
        .context("Failed to clear index")?;
        drop(conn);

        // Emit event
        self.emit(IndexEvent::IndexCleared);

        Ok(())
    }

    /// Get statistics about the index.
    pub fn stats(&self) -> Result<IndexStats> {
        let conn = self.connection.lock().unwrap();

        let node_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))?;
        let file_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE node_type = 0",
            [],
            |row| row.get(0),
        )?;
        let folder_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE node_type = 1",
            [],
            |row| row.get(0),
        )?;
        let offline_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM offline_files", [], |row| row.get(0))?;
        let offline_size: i64 = conn.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM offline_files",
            [],
            |row| row.get(0),
        )?;
        let mutation_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM mutations", [], |row| row.get(0))?;

        Ok(IndexStats {
            node_count,
            file_count,
            folder_count,
            offline_count,
            offline_size,
            mutation_count,
        })
    }
}

/// Statistics about the index.
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub node_count: i64,
    pub file_count: i64,
    pub folder_count: i64,
    pub offline_count: i64,
    pub offline_size: i64,
    pub mutation_count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_operations() {
        let index = OfflineIndex::open_in_memory().unwrap();

        let node = IndexedNode {
            link_id: "link1".to_string(),
            parent_link_id: None,
            volume_id: "vol1".to_string(),
            
            name: "test.txt".to_string(),
            node_type: NodeType::File,
            mime_type: Some("text/plain".to_string()),
            size: Some(1024),
            revision_id: Some("rev1".to_string()),
            creation_time: Utc::now(),
            modification_time: Some(Utc::now()),
            fetched_at: Utc::now(),
            local_only: false,
            pending_delete: false,
        };

        index.upsert_node(&node).unwrap();

        let fetched = index.get_node("link1").unwrap().unwrap();
        assert_eq!(fetched.name, "test.txt");
        assert_eq!(fetched.size, Some(1024));
    }

    #[test]
    fn test_children() {
        let index = OfflineIndex::open_in_memory().unwrap();

        let parent = IndexedNode {
            link_id: "parent".to_string(),
            parent_link_id: None,
            volume_id: "vol1".to_string(),
            
            name: "folder".to_string(),
            node_type: NodeType::Folder,
            mime_type: None,
            size: None,
            revision_id: None,
            creation_time: Utc::now(),
            modification_time: None,
            fetched_at: Utc::now(),
            local_only: false,
            pending_delete: false,
        };

        let child = IndexedNode {
            link_id: "child1".to_string(),
            parent_link_id: Some("parent".to_string()),
            volume_id: "vol1".to_string(),
            
            name: "file.txt".to_string(),
            node_type: NodeType::File,
            mime_type: Some("text/plain".to_string()),
            size: Some(512),
            revision_id: Some("rev1".to_string()),
            creation_time: Utc::now(),
            modification_time: None,
            fetched_at: Utc::now(),
            local_only: false,
            pending_delete: false,
        };

        index.upsert_node(&parent).unwrap();
        index.upsert_node(&child).unwrap();

        let children = index.get_children("parent").unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "file.txt");
    }

    #[test]
    fn test_offline_tracking() {
        let index = OfflineIndex::open_in_memory().unwrap();

        let node = IndexedNode {
            link_id: "link1".to_string(),
            parent_link_id: None,
            volume_id: "vol1".to_string(),
            
            name: "test.txt".to_string(),
            node_type: NodeType::File,
            mime_type: Some("text/plain".to_string()),
            size: Some(1024),
            revision_id: Some("rev1".to_string()),
            creation_time: Utc::now(),
            modification_time: None,
            fetched_at: Utc::now(),
            local_only: false,
            pending_delete: false,
        };

        index.upsert_node(&node).unwrap();

        // Initially not available
        assert_eq!(
            index.get_offline_status("link1").unwrap(),
            OfflineStatus::NotAvailable
        );

        // Mark as offline
        index
            .mark_offline("link1", "rev1", "/cache/file1", 1024)
            .unwrap();
        assert_eq!(
            index.get_offline_status("link1").unwrap(),
            OfflineStatus::Available
        );

        // Update node with new revision - now stale
        let updated = IndexedNode {
            revision_id: Some("rev2".to_string()),
            ..node
        };
        index.upsert_node(&updated).unwrap();
        assert_eq!(
            index.get_offline_status("link1").unwrap(),
            OfflineStatus::Stale
        );
    }

    #[test]
    fn test_mutations() {
        let index = OfflineIndex::open_in_memory().unwrap();

        let mutation = PendingMutation::CreateFile {
            link_id: "new_link".to_string(),
            parent_link_id: "parent".to_string(),
            name: "new_file.txt".to_string(),
            mime_type: "text/plain".to_string(),
            content_path: "/tmp/content".to_string(),
        };

        let id = index.queue_mutation(&mutation).unwrap();
        assert!(id > 0);

        let mutations = index.get_pending_mutations().unwrap();
        assert_eq!(mutations.len(), 1);

        index.remove_mutation(id).unwrap();
        assert_eq!(index.pending_mutation_count().unwrap(), 0);
    }
}
