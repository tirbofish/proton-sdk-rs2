use crate::rusqlite_cache::RusqliteCache;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::NodeUid;
use proton_sdk_rs2::session::ProtonAPISession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Represents the state of the REPL session
pub struct ReplState {
    /// Authenticated session
    session: Option<ProtonAPISession>,
    /// Drive client (requires session)
    client: Option<ProtonDriveClient>,
    /// SQLite cache
    cache: Option<Arc<RusqliteCache>>,
    /// Current working directory path
    current_path: Vec<String>,
    /// Current node UID
    current_node_uid: Option<NodeUid>,
    /// My Files root node UID (for '/' navigation)
    root_node_uid: Option<NodeUid>,
    /// Username of authenticated user
    username: Option<String>,
    /// Current sync status message
    sync_status: Arc<parking_lot::RwLock<Option<String>>>,
    /// Cancellation flag for current operation
    cancelled: AtomicBool,
    /// Active FUSE mount point (for auto-unmount on exit)
    mount_point: Option<PathBuf>,
}

impl ReplState {
    pub fn new() -> Self {
        Self {
            session: None,
            client: None,
            cache: None,
            current_path: vec!["MyFiles".to_string()],
            current_node_uid: None,
            root_node_uid: None,
            username: None,
            sync_status: Arc::new(parking_lot::RwLock::new(None)),
            cancelled: AtomicBool::new(false),
            mount_point: None,
        }
    }

    pub fn set_sync_status(&self, status: Option<String>) {
        *self.sync_status.write() = status;
    }

    pub fn get_sync_status(&self) -> Option<String> {
        self.sync_status.read().clone()
    }

    pub fn set_cache(&mut self, cache: Arc<RusqliteCache>) {
        self.cache = Some(cache);
    }

    pub fn get_cache(&self) -> Option<Arc<RusqliteCache>> {
        self.cache.clone()
    }

    pub fn is_authenticated(&self) -> bool {
        self.session.is_some() && self.client.is_some()
    }

    pub fn set_session(&mut self, session: ProtonAPISession) {
        self.session = Some(session);
    }

    pub fn set_client(&mut self, client: ProtonDriveClient) {
        self.client = Some(client);
    }

    pub fn set_username(&mut self, username: String) {
        self.username = Some(username);
    }

    pub fn get_client(&self) -> Option<&ProtonDriveClient> {
        self.client.as_ref()
    }

    pub fn get_username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    pub fn current_path_display(&self) -> String {
        if self.current_path.is_empty() {
            "/".to_string()
        } else {
            format!("{}/", self.current_path.join("/"))
        }
    }

    pub fn get_current_path(&self) -> &[String] {
        &self.current_path
    }

    pub fn set_current_path(&mut self, path: Vec<String>) {
        self.current_path = path;
    }

    pub fn get_current_node_uid(&self) -> Option<&NodeUid> {
        self.current_node_uid.as_ref()
    }

    pub fn set_current_node_uid(&mut self, uid: NodeUid) {
        self.current_node_uid = Some(uid);
    }

    pub fn clear_current_node_uid(&mut self) {
        self.current_node_uid = None;
    }

    pub fn set_root_node_uid(&mut self, uid: NodeUid) {
        self.root_node_uid = Some(uid);
    }

    pub fn get_root_node_uid(&self) -> Option<&NodeUid> {
        self.root_node_uid.as_ref()
    }

    pub fn clear_session(&mut self) {
        self.session = None;
        self.client = None;
        self.username = None;
        self.current_path = vec!["MyFiles".to_string()];
        self.current_node_uid = None;
        self.root_node_uid = None;
    }

    pub fn clear_cancelled(&self) {
        self.cancelled.store(false, Ordering::Relaxed);
    }

    pub fn set_mount_point(&mut self, path: Option<PathBuf>) {
        self.mount_point = path;
    }

    pub fn get_mount_point(&self) -> Option<&PathBuf> {
        self.mount_point.as_ref()
    }
}
