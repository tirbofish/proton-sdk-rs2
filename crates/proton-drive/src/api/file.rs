pub mod photos;
pub mod thumbnail;

use crate::api::ApiResponse;
use crate::api::block::{BlockUploadPreparationRequest, BlockUploadPreparationResponse};
use crate::api::file::thumbnail::{ThumbnailBlockListRequest, ThumbnailBlockListResponse};
use crate::api::node::NodeCreationRequest;
use crate::api::revision::{
    ActiveRevisionDto, RevisionCreationRequest, RevisionCreationResponse, RevisionResponse,
    RevisionUpdateRequest,
};
use crate::links::LinkId;
use crate::pgp::PgpArmoredSignature;
use crate::revision::RevisionId;
use crate::volume::VolumeId;
use async_trait::async_trait;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FileContentDigestsDto {
    #[serde(rename = "SHA1")]
    #[serde(default, with = "crate::utils::serde::forgiving_hex_bytes_opt")]
    pub sha1: Option<Vec<u8>>,
}

#[derive(Debug, Deserialize)]
pub struct FileCreationIdentifiers {
    #[serde(rename = "ID")]
    pub link_id: LinkId,

    #[serde(rename = "RevisionID")]
    pub revision_id: RevisionId,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct FileCreationRequest {
    #[serde(flatten)]
    pub base: NodeCreationRequest,

    #[serde(rename = "MIMEType")]
    pub media_type: String,

    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub content_key_packet: Vec<u8>,

    #[serde(rename = "ContentKeyPacketSignature")]
    pub content_key_signature: PgpArmoredSignature,

    #[serde(rename = "ClientUID")]
    pub client_uid: Option<String>,

    pub intended_upload_size: Option<i64>,

    #[serde(rename = "SignatureAddress")]
    pub signature_email_address: String,
}

#[derive(Debug, Deserialize)]
pub struct FileCreationResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "File")]
    pub identifiers: FileCreationIdentifiers,
}

impl FileCreationResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FileDto {
    pub media_type: String,

    #[serde(rename = "TotalEncryptedSize")]
    pub total_size_on_storage: i64,

    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub content_key_packet: Vec<u8>,

    #[serde(rename = "ContentKeyPacketSignature")]
    pub content_key_signature: Option<PgpArmoredSignature>,

    pub active_revision: Option<ActiveRevisionDto>,
}

#[async_trait]
pub trait FilesApiClient: Send + Sync {
    async fn create_file(
        &self,
        volume_id: VolumeId,
        request: FileCreationRequest,
    ) -> anyhow::Result<FileCreationResponse>;

    async fn create_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: RevisionCreationRequest,
    ) -> anyhow::Result<RevisionCreationResponse>;

    async fn prepare_block_upload(
        &self,
        request: BlockUploadPreparationRequest,
    ) -> anyhow::Result<BlockUploadPreparationResponse>;

    async fn update_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        revision_id: RevisionId,
        request: RevisionUpdateRequest,
    ) -> anyhow::Result<ApiResponse>;

    async fn get_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        revision_id: RevisionId,
        from_block_index: Option<i32>,
        page_size: Option<i32>,
        without_block_urls: bool,
    ) -> anyhow::Result<RevisionResponse>;

    async fn get_thumbnail_blocks(
        &self,
        volume_id: VolumeId,
        thumbnail_ids: Vec<String>,
    ) -> anyhow::Result<ThumbnailBlockListResponse>;

    async fn delete_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        revision_id: RevisionId,
    ) -> anyhow::Result<ApiResponse>;
}
use proton_sdk_rs2::auth::TokenCredential;

pub struct DefaultFilesApiClient {
    client: ClientWithMiddleware,
    base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultFilesApiClient {
    pub fn new(
        client: ClientWithMiddleware,
        base_url: reqwest::Url,
        token_credential: Option<TokenCredential>,
    ) -> Self {
        Self {
            client,
            base_url,
            token_credential,
        }
    }

    async fn add_auth_headers(
        &self,
        mut builder: reqwest_middleware::RequestBuilder,
    ) -> anyhow::Result<reqwest_middleware::RequestBuilder> {
        if let Some(credential) = &self.token_credential {
            let (access_token, _) = credential.get_tokens().await?;
            builder = builder.header("Authorization", format!("Bearer {}", access_token));
            builder = builder.header("x-pm-uid", credential.session_id().raw());
        }
        Ok(builder)
    }
}

#[async_trait]
impl FilesApiClient for DefaultFilesApiClient {
    #[tracing::instrument(skip(self, request))]
    async fn create_file(
        &self,
        volume_id: VolumeId,
        request: FileCreationRequest,
    ) -> anyhow::Result<FileCreationResponse> {
        tracing::debug!(volume_id = %volume_id.raw(), "Creating file");
        let url = self
            .base_url
            .join(&format!("v2/volumes/{}/files", volume_id.raw()))?;
        let builder = self.client.post(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        let resp = builder.send().await?.json::<FileCreationResponse>().await?;
        tracing::info!(link_id = %resp.identifiers.link_id.raw(), revision_id = %resp.identifiers.revision_id.raw(), "File created");
        Ok(resp)
    }

    #[tracing::instrument(skip(self, request))]
    async fn create_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: RevisionCreationRequest,
    ) -> anyhow::Result<RevisionCreationResponse> {
        tracing::debug!(volume_id = %volume_id.raw(), link_id = %link_id.raw(), "Creating revision");
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/files/{}/revisions",
            volume_id.raw(),
            link_id.raw()
        ))?;
        let builder = self.client.post(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        let resp = builder
            .send()
            .await?
            .json::<RevisionCreationResponse>()
            .await?;
        tracing::info!(revision_id = %resp.identity.revision_id.raw(), "Revision created");
        Ok(resp)
    }

    #[tracing::instrument(skip(self, request))]
    async fn prepare_block_upload(
        &self,
        request: BlockUploadPreparationRequest,
    ) -> anyhow::Result<BlockUploadPreparationResponse> {
        tracing::debug!("Preparing block upload");
        let url = self.base_url.join("blocks")?;
        let builder = self.client.post(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        let resp = builder
            .send()
            .await?
            .json::<BlockUploadPreparationResponse>()
            .await?;
        tracing::info!(targets = resp.upload_targets.len(), "Block upload prepared");
        Ok(resp)
    }

    #[tracing::instrument(skip(self, request))]
    async fn update_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        revision_id: RevisionId,
        request: RevisionUpdateRequest,
    ) -> anyhow::Result<ApiResponse> {
        tracing::debug!(volume_id = %volume_id.raw(), link_id = %link_id.raw(), revision_id = %revision_id.raw(), "Sealing revision");
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/files/{}/revisions/{}",
            volume_id.raw(),
            link_id.raw(),
            revision_id.raw()
        ))?;
        let builder = self.client.put(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        let resp = builder.send().await?.json::<ApiResponse>().await?;
        if resp.is_success() {
            tracing::info!("Revision sealed successfully");
        } else {
            tracing::warn!(code = resp.code.0, error = ?resp.error_message, "Revision sealing failed");
        }
        Ok(resp)
    }

    async fn get_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        revision_id: RevisionId,
        from_block_index: Option<i32>,
        page_size: Option<i32>,
        without_block_urls: bool,
    ) -> anyhow::Result<RevisionResponse> {
        let mut path = format!(
            "v2/volumes/{}/files/{}/revisions/{}?",
            volume_id.raw(),
            link_id.raw(),
            revision_id.raw()
        );

        if let Some(idx) = from_block_index {
            path.push_str(&format!("FromBlockIndex={}&", idx));
        }

        if let Some(size) = page_size {
            path.push_str(&format!("PageSize={}&", size));
        }

        path.push_str(&format!(
            "NoBlockUrls={}",
            if without_block_urls { 1 } else { 0 }
        ));

        let url = self.base_url.join(&path)?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;

        Ok(builder.send().await?.json::<RevisionResponse>().await?)
    }

    async fn delete_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        revision_id: RevisionId,
    ) -> anyhow::Result<ApiResponse> {
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/files/{}/revisions/{}",
            volume_id.raw(),
            link_id.raw(),
            revision_id.raw()
        ))?;
        let builder = self.client.delete(url);
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder.send().await?.json::<ApiResponse>().await?)
    }

    async fn get_thumbnail_blocks(
        &self,
        volume_id: VolumeId,
        thumbnail_ids: Vec<String>,
    ) -> anyhow::Result<ThumbnailBlockListResponse> {
        let url = self
            .base_url
            .join(&format!("volumes/{}/thumbnails", volume_id.raw()))?;
        let builder = self
            .client
            .post(url)
            .json(&ThumbnailBlockListRequest { thumbnail_ids });
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder
            .send()
            .await?
            .json::<ThumbnailBlockListResponse>()
            .await?)
    }
}
