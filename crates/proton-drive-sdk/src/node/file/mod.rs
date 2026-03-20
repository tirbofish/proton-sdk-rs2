pub mod download;
pub mod upload;

use crate::client::ProtonDriveClient;
use crate::error::ProtonDriveError;
use crate::meta::AdditionalMetadataProperty;
use crate::node::DegradedNodeBase;
use crate::node::revision::{DegradedRevision, Revision};
use crate::node::{DegradedNodeSecrets, NodeSecrets};
use crate::node::{NodeUid, thumbnail::ThumbnailType};
use crate::pgp::PgpSessionKey;
use crate::share::ShareId;
use crate::utils::PotentialObject;
use chrono::{DateTime, Utc};

pub struct FileOperations;

impl FileOperations {
    pub async fn get_secrets(
        client: &ProtonDriveClient,
        file_uid: NodeUid,
    ) -> anyhow::Result<FileSecrets> {
        if let Some(secrets) = client
            .cache()
            .secrets()
            .try_get_file_secrets(file_uid.clone())
            .await?
        {
            return secrets.result().map_err(|e| anyhow::anyhow!(e.to_string()));
        }

        let metadata_result =
            crate::node::operations::NodeOperations::get_node_metadata(client, file_uid).await?;
        let metadata = metadata_result.result()?;

        match metadata.try_get_file_else_folder() {
            Ok((_, secrets)) => Ok(secrets),
            Err(_) => anyhow::bail!("Expected file, got folder"),
        }
    }

    pub async fn enumerate_thumbnails(
        _client: &ProtonDriveClient,
        _file_uids: Vec<NodeUid>,
        _thumbnail_type: ThumbnailType,
    ) -> anyhow::Result<Vec<FileThumbnail>> {
        // TODO: Implement enumerate_thumbnails
        todo!("Implement enumerate_thumbnails")
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct DegradedFileNode {
    #[serde(flatten)]
    pub base: DegradedNodeBase,
    pub media_type: String,
    pub active_revision: Option<DegradedRevision>,
    pub total_storage_quota_usage: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DegradedFileSecrets {
    pub base: DegradedNodeSecrets,
    pub content_key: Option<PgpSessionKey>,
}

#[derive(Debug, Clone)]
pub struct DegradedFileMetadata {
    pub node: DegradedFileNode,
    pub secrets: DegradedFileSecrets,
    pub membership_share_id: Option<ShareId>,
    pub name_hash_digest: Vec<u8>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct FileContentDigests {
    pub sha1: Option<Vec<u8>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileOrFileDraftNode {
    #[serde(flatten)]
    pub base: crate::node::NodeBase,
    /// The file type in the format of a MIME type
    pub media_type: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileNode {
    #[serde(flatten)]
    pub base: FileOrFileDraftNode,
    pub active_revision: Revision,
    pub total_size_on_cloud_storage: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileDraftNode {
    #[serde(flatten)]
    pub base: FileOrFileDraftNode,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileSecrets {
    pub base: NodeSecrets,
    pub content_key: PgpSessionKey,
}

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub node: FileNode,
    pub secrets: FileSecrets,
    pub membership_share_id: Option<ShareId>,
    pub name_hash_digest: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct FileThumbnail {
    pub file_uid: NodeUid,
    pub result: PotentialObject<Vec<u8>, ProtonDriveError>,
}

#[derive(Debug, Clone)]
pub struct FileUploadMetadata {
    pub last_modification_time: Option<DateTime<Utc>>,
    pub additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
}
