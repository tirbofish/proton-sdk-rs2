use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::NodeUid;
use proton_sdk_rs2::session::ProtonAPISession;
use std::sync::atomic::{AtomicBool, Ordering};

/// Represents the state of the REPL session
pub struct ReplState {
    /// Authenticated session
    session: Option<ProtonAPISession>,
    /// Drive client (requires session)
    client: Option<ProtonDriveClient>,
    /// Current working directory path
    current_path: Vec<String>,
    /// Current node UID
    current_node_uid: Option<NodeUid>,
    /// My Files root node UID (for '/' navigation)
    root_node_uid: Option<NodeUid>,
    /// Username of authenticated user
    username: Option<String>,
    /// Cancellation flag for current operation
    cancelled: AtomicBool,
}

impl ReplState {
    pub fn new() -> Self {
        Self {
            session: None,
            client: None,
            current_path: vec!["MyFiles".to_string()],
            current_node_uid: None,
            root_node_uid: None,
            username: None,
            cancelled: AtomicBool::new(false),
        }
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

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn set_cancelled(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn clear_cancelled(&self) {
        self.cancelled.store(false, Ordering::Relaxed);
    }
}
