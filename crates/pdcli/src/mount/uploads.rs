use chrono::{DateTime, Utc};

use proton_drive_sdk::node::NodeUid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct PendingFile {
    #[allow(dead_code)]
    pub(super) parent_inode: u64,
    pub(super) parent_uid: NodeUid,
    pub(super) name: String,
    pub(super) mime_type: String,
    pub(super) content: Vec<u8>,
    #[allow(dead_code)]
    pub(super) creation_time: DateTime<Utc>,
    pub(super) dirty: bool,
    #[serde(default)]
    pub(super) local_only: bool,
}

#[derive(Debug)]
pub(super) struct WriteBuffer {
    pub(super) inode: u64,
    pub(super) is_new: bool,
    pub(super) offset: u64,
    pub(super) content: Vec<u8>,
    pub(super) dirty: bool,
}
