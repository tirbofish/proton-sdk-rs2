use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use crate::links::LinkId;
use crate::node::{NodeUid};
use crate::revision::RevisionId;
use crate::volume::VolumeId;
use chrono::{DateTime, Utc};
use crate::node::file::FileContentDigests;
use crate::protobuf::ThumbnailHeader;
use crate::meta::AdditionalMetadataProperty;
use crate::utils::PotentialObject;
use crate::author::Author;
use crate::protobuf::SignatureVerificationError;
use crate::error::ProtonDriveError;
use crate::client::ProtonDriveClient;
use crate::node::draft::RevisionDraft;
use crate::node::download::DownloadState;
use std::sync::Arc;

pub const REVISION_WRITER_DEFAULT_BLOCK_SIZE: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct DegradedRevision {
    pub uid: RevisionUid,
    pub creation_time: DateTime<Utc>,
    pub size_on_cloud_storage: i64,
    pub claimed_size: Option<i64>,
    pub claimed_digests: Option<FileContentDigests>,
    pub claimed_modification_time: Option<DateTime<Utc>>,
    pub thumbnails: Vec<ThumbnailHeader>,
    pub additional_claimed_metadata: Option<Vec<AdditionalMetadataProperty>>,
    pub content_author: Option<PotentialObject<Author, SignatureVerificationError>>,
    pub can_decrypt: bool,
    pub errors: Vec<ProtonDriveError>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Revision {
    pub uid: RevisionUid,
    pub creation_time: DateTime<Utc>,
    pub size_on_cloud_storage: i64,
    pub claimed_size: Option<i64>,
    pub claimed_digests: FileContentDigests,
    pub claimed_modification_time: Option<DateTime<Utc>>,
    pub thumbnails: Vec<ThumbnailHeader>,
    pub additional_claimed_metadata: Option<Vec<AdditionalMetadataProperty>>,
    pub content_author: Option<PotentialObject<Author, SignatureVerificationError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum RevisionState {
    Draft      = 0,
    Active     = 1,
    Superseded = 2,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(try_from = "String", into = "String")]
pub struct RevisionUid {
    pub node_uid: NodeUid,
    pub revision_id: RevisionId,
}

impl RevisionUid {
    pub fn new(node_uid: NodeUid, revision_id: RevisionId) -> Self {
        Self { node_uid, revision_id }
    }

    pub fn from_parts(volume_id: VolumeId, link_id: LinkId, revision_id: RevisionId) -> Self {
        Self {
            node_uid: NodeUid::new(volume_id, link_id),
            revision_id,
        }
    }

    pub fn try_parse(s: &str) -> Option<Self> {
        // format: {volumeId}~{linkId}~{revisionId}
        let mut parts = s.rsplitn(2, '~');
        let revision_id = parts.next()?;
        let node_uid_str = parts.next()?;
        let node_uid = NodeUid::try_parse(node_uid_str)?;
        Some(Self {
            node_uid,
            revision_id: RevisionId::new(revision_id.to_string()),
        })
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        Self::try_parse(s).ok_or_else(|| format!("Invalid revision UID format: \"{}\"", s))
    }

    pub fn deconstruct(self) -> (NodeUid, RevisionId) {
        (self.node_uid, self.revision_id)
    }
}

impl std::fmt::Display for RevisionUid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}~{}", self.node_uid, self.revision_id.raw())
    }
}

impl From<RevisionUid> for String {
    fn from(uid: RevisionUid) -> Self {
        uid.to_string()
    }
}

impl TryFrom<String> for RevisionUid {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        RevisionUid::parse(&s)
    }
}

pub struct RevisionOperations;

impl RevisionOperations {
    pub async fn open_for_writing(
        client: &ProtonDriveClient,
        draft: RevisionDraft,
        release_blocks_action: Box<dyn Fn(i32) + Send + Sync>,
    ) -> anyhow::Result<RevisionWriter> {
        client.block_uploader().queue.start_file().await?;

        Ok(RevisionWriter {
            client: Arc::new(client.clone()),
            draft,
            release_blocks_action,
            finish_file_action: Box::new({
                let client = client.clone();
                move || { client.block_uploader().queue.finish_file(); }
            }),
            target_block_size: client.target_block_size(),
        })
    }

    pub async fn create_download_state(
        client: &ProtonDriveClient,
        revision_uid: RevisionUid,
        release_block_listing_action: Box<dyn Fn(i32) + Send + Sync>,
    ) -> anyhow::Result<DownloadState> {
        let _node_metadata = crate::node::operations::NodeOperations::get_node(client, revision_uid.node_uid.clone()).await?;
        let secrets = crate::node::file::FileOperations::get_secrets(client, revision_uid.node_uid.clone()).await?;

        let revision_response = client.api().files().get_revision(
            revision_uid.node_uid.volume_id.clone(),
            revision_uid.node_uid.link_id.clone(),
            revision_uid.revision_id.clone(),
            Some(crate::node::download::MIN_BLOCK_INDEX),
            Some(crate::node::download::DEFAULT_BLOCK_PAGE_SIZE),
            false,
        ).await?;

        release_block_listing_action(1);

        Ok(DownloadState::new(
            revision_uid,
            revision_response.revision,
            secrets.base.key,
            secrets.content_key,
        ))
    }

    pub fn open_for_reading(
        client: &ProtonDriveClient,
        download_state: Arc<DownloadState>,
        release_block_listing_action: Box<dyn Fn(i32) + Send + Sync>,
    ) -> crate::node::download::RevisionReader {
        crate::node::download::RevisionReader::new(
            Arc::new(client.clone()),
            download_state,
            release_block_listing_action,
            Box::new({
                let client = client.clone();
                move || { client.block_downloader().queue.finish_file(); }
            })
        )
    }
}

pub struct RevisionWriter {
    client: Arc<ProtonDriveClient>,
    draft: RevisionDraft,
    release_blocks_action: Box<dyn Fn(i32) + Send + Sync>,
    finish_file_action: Box<dyn Fn() + Send + Sync>,
    target_block_size: usize,
}

impl Drop for RevisionWriter {
    fn drop(&mut self) {
        (self.finish_file_action)();
    }
}
