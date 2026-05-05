pub mod photos;
pub mod thumbnail;

use crate::api::ApiResponse;
use crate::api::block::{BlockUploadPreparationRequest, BlockUploadPreparationResponse};
use crate::api::file::thumbnail::{ThumbnailBlockListRequest, ThumbnailBlockListResponse};
use crate::api::node::NodeCreationRequest;
use crate::api::revision::{
    ActiveRevisionDto, RevisionCreationRequest, RevisionCreationResponse, RevisionListResponse,
    RevisionResponse, RevisionUpdateRequest,
};
use crate::links::LinkId;
use crate::pgp::PgpArmoredSignature;
use crate::revision::RevisionId;
use crate::volume::VolumeId;
use anyhow::Context;
use async_trait::async_trait;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
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

    async fn get_revisions(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
    ) -> anyhow::Result<RevisionListResponse>;

    async fn restore_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        revision_id: RevisionId,
    ) -> anyhow::Result<ApiResponse>;

    async fn delete_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        revision_id: RevisionId,
    ) -> anyhow::Result<ApiResponse>;
}
use proton_sdk_rs2::auth::TokenCredential;

/// Parse an HTTP response as JSON, including the raw body text in the error
/// message on failure so that Proton error pages are visible in logs.
async fn parse_json_response<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> anyhow::Result<T> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Try to extract a Proton error message from the JSON body.
        if let Ok(api) = serde_json::from_str::<ApiResponse>(&body) {
            if let Some(msg) = &api.error_message {
                return Err(anyhow::anyhow!("API error {}: {}", api.code.0, msg));
            }
        }
        return Err(anyhow::anyhow!("HTTP {}: {}", status, body));
    }
    serde_json::from_str::<T>(&body).map_err(|e| {
        anyhow::anyhow!(
            "JSON parse error: {}. Body: {}",
            e,
            &body[..body.len().min(512)]
        )
    })
}

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
        let resp = parse_json_response::<FileCreationResponse>(builder.send().await?).await?;
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
        let resp = parse_json_response::<RevisionCreationResponse>(builder.send().await?).await?;
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
        let resp =
            parse_json_response::<BlockUploadPreparationResponse>(builder.send().await?).await?;
        tracing::info!(
            blocks = resp.upload_targets.len(),
            thumbnails = resp.thumbnail_upload_targets.len(),
            "Block upload prepared"
        );
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
        let resp = parse_json_response::<ApiResponse>(builder.send().await?).await?;
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

    async fn get_revisions(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
    ) -> anyhow::Result<RevisionListResponse> {
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/files/{}/revisions",
            volume_id.raw(),
            link_id.raw(),
        ))?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;
        parse_json_response::<RevisionListResponse>(builder.send().await?).await
    }

    async fn restore_revision(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        revision_id: RevisionId,
    ) -> anyhow::Result<ApiResponse> {
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/files/{}/revisions/{}/restore",
            volume_id.raw(),
            link_id.raw(),
            revision_id.raw(),
        ))?;
        let builder = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_LENGTH, 0);
        let builder = self.add_auth_headers(builder).await?;
        parse_json_response::<ApiResponse>(builder.send().await?).await
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
        parse_json_response::<ApiResponse>(builder.send().await?).await
    }

    async fn get_thumbnail_blocks(
        &self,
        volume_id: VolumeId,
        thumbnail_ids: Vec<String>,
    ) -> anyhow::Result<ThumbnailBlockListResponse> {
        let url = self
            .base_url
            .join(&format!("volumes/{}/thumbnails", volume_id.raw()))?;

        tracing::debug!(
            "Fetching thumbnail blocks: url={}, thumbnail_ids={:?}",
            url,
            thumbnail_ids
        );

        let builder = self.client.post(url).json(&ThumbnailBlockListRequest {
            thumbnail_ids: thumbnail_ids.clone(),
        });
        let builder = self.add_auth_headers(builder).await?;
        let response = builder.send().await?;

        let status = response.status();
        let body_text = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read body>".to_string());

        if !status.is_success() {
            tracing::warn!(
                "Thumbnail blocks API returned error: status={}, body={}",
                status,
                body_text
            );
            anyhow::bail!(
                "Thumbnail blocks API failed with status {}: {}",
                status,
                body_text
            );
        }

        tracing::debug!("Thumbnail blocks API response: {}", body_text);

        let parsed: ThumbnailBlockListResponse = serde_json::from_str(&body_text)
            .with_context(|| format!("Failed to parse thumbnail blocks response: {}", body_text))?;

        tracing::debug!("Parsed {} thumbnail blocks", parsed.blocks.len());

        Ok(parsed)
    }
}
