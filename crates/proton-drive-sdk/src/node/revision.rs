use crate::author::Author;
use crate::client::ProtonDriveClient;
use crate::error::ProtonDriveError;
use crate::links::LinkId;
use crate::meta::AdditionalMetadataProperty;
use crate::node::NodeUid;
use crate::node::download::DownloadState;
use crate::node::draft::RevisionDraft;
use crate::node::file::FileContentDigests;
use crate::protobuf::SignatureVerificationError;
use crate::protobuf::ThumbnailHeader;
use crate::revision::RevisionId;
use crate::utils::PotentialObject;
use crate::volume::VolumeId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use sha1::Digest;
use proton_rpgp::pgp::ser::Serialize as _;
use std::sync::Arc;
use tokio::io::AsyncReadExt;

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
    Draft = 0,
    Active = 1,
    Superseded = 2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionInfo {
    pub uid: RevisionUid,
    pub state: RevisionState,
    pub creation_time: DateTime<Utc>,
    pub size_on_cloud_storage: i64,
    pub claimed_size: Option<i64>,
    pub claimed_modification_time: Option<DateTime<Utc>>,
    pub claimed_sha1: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(try_from = "String", into = "String")]
pub struct RevisionUid {
    pub node_uid: NodeUid,
    pub revision_id: RevisionId,
}

impl RevisionUid {
    pub fn new(node_uid: NodeUid, revision_id: RevisionId) -> Self {
        Self {
            node_uid,
            revision_id,
        }
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
        expected_size: i64,
        last_modification_time: Option<DateTime<Utc>>,
        additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
        media_info: Option<crate::api::attr::MediaExtendedAttributes>,
    ) -> anyhow::Result<RevisionWriter> {
        let file_permit = client.block_uploader().queue.start_file().await?;

        Ok(RevisionWriter {
            client: Arc::new(client.clone()),
            draft,
            release_blocks_action,
            _file_permit: file_permit,
            target_block_size: client.target_block_size(),
            total_written: 0,
            block_number: 1,
            digests: Vec::new(),
            block_sizes: Vec::new(),
            thumbnail_digests: Vec::new(),
            sha1_hasher: sha1::Sha1::default(),
            expected_size,
            last_modification_time,
            additional_metadata,
            media_info,
        })
    }

    pub async fn create_download_state(
        client: &ProtonDriveClient,
        revision_uid: RevisionUid,
        release_block_listing_action: Box<dyn Fn(i32) + Send + Sync>,
    ) -> anyhow::Result<DownloadState> {
        let _node_metadata = crate::node::operations::NodeOperations::get_node(
            client,
            revision_uid.node_uid.clone(),
        )
        .await?;
        let secrets =
            crate::node::file::FileOperations::get_secrets(client, revision_uid.node_uid.clone())
                .await?;

        let revision_response = client
            .api()
            .files()
            .get_revision(
                revision_uid.node_uid.volume_id.clone(),
                revision_uid.node_uid.link_id.clone(),
                revision_uid.revision_id.clone(),
                Some(crate::node::download::MIN_BLOCK_INDEX),
                Some(crate::node::download::DEFAULT_BLOCK_PAGE_SIZE),
                false,
            )
            .await?;

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
        )
    }
}

pub struct RevisionWriter {
    client: Arc<ProtonDriveClient>,
    draft: RevisionDraft,
    release_blocks_action: Box<dyn Fn(i32) + Send + Sync>,
    _file_permit: tokio::sync::OwnedSemaphorePermit,
    target_block_size: usize,
    total_written: i64,
    block_number: i32,
    digests: Vec<u8>,
    block_sizes: Vec<i32>,
    thumbnail_digests: Vec<u8>,
    sha1_hasher: sha1::Sha1,
    expected_size: i64,
    last_modification_time: Option<DateTime<Utc>>,
    additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
    media_info: Option<crate::api::attr::MediaExtendedAttributes>,
}

impl RevisionWriter {
    #[tracing::instrument(skip(self, content_stream, on_progress))]
    pub async fn write(
        &mut self,
        mut content_stream: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        on_progress: Arc<dyn Fn(i64, i64) + Send + Sync>,
    ) -> anyhow::Result<()> {
        tracing::debug!(expected_size = self.expected_size, "Starting write");
        let mut buffer = vec![0u8; self.target_block_size];

        loop {
            let mut n = 0;
            while n < self.target_block_size {
                let read_bytes = content_stream.read(&mut buffer[n..]).await?;
                if read_bytes == 0 {
                    break;
                }
                n += read_bytes;
            }

            if n == 0 {
                break;
            }

            let block_data = &buffer[..n];
            sha1::Digest::update(&mut self.sha1_hasher, block_data);

            let on_block_progress: Box<dyn Fn(i64) + Send + Sync> = Box::new({
                let on_progress = on_progress.clone();
                let current_total = self.total_written;
                let expected_size = self.expected_size;
                move |progress| {
                    on_progress(current_total + progress, expected_size);
                }
            });

            let result = self
                .client
                .block_uploader()
                .upload_content(
                    &self.client,
                    &self.draft,
                    self.block_number,
                    block_data,
                    Some(&on_block_progress),
                )
                .await?;

            self.digests.extend_from_slice(&result.sha256_digest);
            self.block_sizes.push(n as i32);
            self.total_written += n as i64;
            self.block_number += 1;

            (self.release_blocks_action)(1);
        }
        Ok(())
    }
    pub async fn upload_thumbnails(
        &mut self,
        thumbnails: Vec<crate::node::thumbnail::Thumbnail>,
    ) -> anyhow::Result<()> {
        use crate::api::block::BlockUploadPreparationRequest;
        use crate::api::file::thumbnail::ThumbnailCreationRequest;
        use sha2::{Digest, Sha256};
        use proton_rpgp::Encryptor;

        if thumbnails.is_empty() {
            return Ok(());
        }

        let mut requests = Vec::new();
        let mut encrypted_thumbnails = Vec::new();

        let sk = self.draft.content_key.to_rpgp_sk()?;

        for thumb in thumbnails {
            let encryptor = Encryptor::default()
                .with_session_key(sk.clone())
                .with_signing_key(&self.draft.signing_key.0);

            // 1. Encrypt the thumbnail data
            let result = encryptor.encrypt(&thumb.content)?;
            let encrypted_data = result.to_bytes()?;

            // 2. Compute SHA256 of the ENCRYPTED data
            let mut hasher = Sha256::new();
            hasher.update(&encrypted_data);
            let hash_digest = hasher.finalize().to_vec();

            requests.push(ThumbnailCreationRequest {
                size: encrypted_data.len() as i32,
                r#type: thumb.r#type,
                hash_digest: hash_digest.clone(),
            });
            encrypted_thumbnails.push((thumb.r#type, encrypted_data, hash_digest));
        }

        // 3. Prepare thumbnails upload
        let request = BlockUploadPreparationRequest {
            address_id: crate::account::AddressId::new(
                self.draft.membership_address.address_id.clone(),
            ),
            volume_id: self.draft.uid.node_uid.volume_id.clone(),
            link_id: self.draft.uid.node_uid.link_id.clone(),
            revision_id: self.draft.uid.revision_id.clone(),
            blocks: vec![],
            thumbnails: requests,
        };

        let response = self
            .client
            .api()
            .files()
            .prepare_block_upload(request)
            .await?;
        
        if response.thumbnail_upload_targets.len() != encrypted_thumbnails.len() {
            anyhow::bail!("Mismatch in received thumbnail upload targets");
        }

        for (target, (thumb_type, encrypted_data, hash_digest)) in response.thumbnail_upload_targets.iter().zip(encrypted_thumbnails) {
            tracing::info!(
                thumbnail_type = ?thumb_type,
                url = %target.base.bare_url,
                "Uploading thumbnail blob"
            );

            // 4. Upload thumbnail blob
            self.client
                .api()
                .storage()
                .upload_blob(
                    &target.base.bare_url,
                    &target.base.token,
                    bytes::Bytes::from(encrypted_data),
                )
                .await?;

            // 5. Store digest for manifest
            self.thumbnail_digests.extend_from_slice(&hash_digest);
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn commit(&mut self) -> anyhow::Result<()> {
        use crate::pgp::{PgpArmoredMessage, PgpArmoredSignature};
        use proton_rpgp::DataEncoding;
        use proton_rpgp::{Encryptor, Signer};

        tracing::info!(
            total_written = self.total_written,
            blocks = self.block_sizes.len(),
            thumbnail_digests = self.thumbnail_digests.len(),
            content_digests = self.digests.len(),
            "Committing revision"
        );
        let signer = Signer::default().with_signing_key(&self.draft.signing_key.0);
        
        let mut manifest = Vec::with_capacity(self.thumbnail_digests.len() + self.digests.len());
        manifest.extend_from_slice(&self.thumbnail_digests);
        manifest.extend_from_slice(&self.digests);

        let manifest_signature = PgpArmoredSignature(String::from_utf8(
            signer.sign_detached(&manifest, DataEncoding::Armored)?,
        )?);

        // 2. Prepare Extended Attributes
        let sha1_digest = self.sha1_hasher.clone().finalize().to_vec();
        let mut additional_metadata = std::collections::HashMap::new();
        if let Some(meta) = &self.additional_metadata {
            for prop in meta {
                additional_metadata.insert(prop.key.clone(), serde_json::Value::String(prop.value.clone()));
            }
        }

        let xattr = crate::api::attr::ExtendedAttributes {
            common: Some(crate::api::attr::CommonExtendedAttributes {
                size: Some(self.total_written),
                modification_time: self.last_modification_time,
                block_sizes: Some(self.block_sizes.clone()),
                digests: Some(crate::api::file::FileContentDigestsDto { sha1: Some(sha1_digest) }),
            }),
            media: self.media_info.clone(),
            additional_metadata,
        };

        let xattr_json = serde_json::to_vec(&xattr)?;

        // 3. Encrypt Extended Attributes
        use proton_rpgp::AsPublicKeyRef;
        let xattr_encryptor = Encryptor::default()
            .with_encryption_key(self.draft.file_key.0.as_public_key())
            .with_signing_key(&self.draft.signing_key.0);
        let xattr_result = xattr_encryptor.encrypt(&xattr_json)?;
        let encrypted_xattr = PgpArmoredMessage(String::from_utf8(xattr_result.armor()?)?);

        // 4. Update Revision
        let request = crate::api::revision::RevisionUpdateRequest {
            manifest_signature,
            signature_email_address: self.draft.membership_address.email_address.clone(),
            extended_attributes: Some(encrypted_xattr),
            photos_attributes: None,
        };

        self.client
            .api()
            .files()
            .update_revision(
                self.draft.uid.node_uid.volume_id.clone(),
                self.draft.uid.node_uid.link_id.clone(),
                self.draft.uid.revision_id.clone(),
                request,
            )
            .await?;

        Ok(())
    }
}
