use proton_drive_sdk::node::NodeUid;

/// A pending mutation that the FUSE layer submitted to the index. A background
/// worker picks these up and executes them against the Proton Drive API.
/// When offline, requests stay in `Pending` state and pile up until
/// connectivity is restored, at which point the worker drains the queue in
/// submission order.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub id: u64,
    pub kind: RequestKind,
    pub status: RequestStatus,
    /// How many times this request has been attempted and failed transiently.
    pub attempts: u32,
}

#[derive(Debug, Clone)]
pub enum RequestKind {
    /// Delete a node (move to trash).
    Delete { node_uid: NodeUid },
    /// Rename a node.
    Rename {
        node_uid: NodeUid,
        new_name: String,
    },
    // Future: CreateFolder, UploadFile, Move, etc.
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestStatus {
    /// Waiting to be picked up by the worker.
    Pending,
    /// Currently being executed against the API.
    InProgress,
    /// A transient failure (e.g. network timeout). Will be retried when
    /// connectivity is restored.
    AwaitingRetry(String),
    /// A permanent failure (e.g. 404, 409). Will not be retried.
    Failed(String),
}
