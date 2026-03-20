use crate::account::{AddressId, AddressKeyId};
use crate::api::ApiResponse;
use crate::links::LinkId;
use crate::pgp::{PgpArmoredMessage, PgpArmoredPrivateKey, PgpArmoredSignature};
use crate::share::ShareId;
use crate::volume::{VolumeId, VolumeState, VolumeType};
use async_trait::async_trait;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};

use proton_sdk_rs2::auth::TokenCredential;

#[async_trait]
pub trait VolumesApiClient: Send + Sync {
    async fn create_volume(
        &self,
        request: VolumeCreationRequest,
    ) -> anyhow::Result<VolumeCreationResponse>;

    async fn get_volume(&self, volume_id: VolumeId) -> anyhow::Result<VolumeResponse>;
}

pub struct DefaultVolumesApiClient {
    client: ClientWithMiddleware,
    base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultVolumesApiClient {
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
}

#[async_trait]
impl VolumesApiClient for DefaultVolumesApiClient {
    async fn create_volume(
        &self,
        request: VolumeCreationRequest,
    ) -> anyhow::Result<VolumeCreationResponse> {
        let url = self.base_url.join("volumes")?;
        let mut request_builder = self.client.post(url).json(&request);

        if let Some(credential) = &self.token_credential {
            let (access_token, _) = credential.get_tokens().await?;
            request_builder =
                request_builder.header("Authorization", format!("Bearer {}", access_token));
            request_builder = request_builder.header("x-pm-uid", credential.session_id().raw());
        }

        let response = request_builder
            .send()
            .await?
            .json::<VolumeCreationResponse>()
            .await?;

        Ok(response)
    }

    async fn get_volume(&self, volume_id: VolumeId) -> anyhow::Result<VolumeResponse> {
        let url = self
            .base_url
            .join(&format!("volumes/{}", volume_id.raw()))?;
        let mut request_builder = self.client.get(url);

        if let Some(credential) = &self.token_credential {
            let (access_token, _) = credential.get_tokens().await?;
            request_builder =
                request_builder.header("Authorization", format!("Bearer {}", access_token));
            request_builder = request_builder.header("x-pm-uid", credential.session_id().raw());
        }

        let response = request_builder
            .send()
            .await?
            .json::<VolumeResponse>()
            .await?;

        Ok(response)
    }
}

#[derive(Debug, Deserialize)]
pub struct ShareTrashDto {
    #[serde(rename = "ShareID")]
    pub share_id: ShareId,

    #[serde(rename = "LinkIDs")]
    pub link_ids: Vec<LinkId>,

    #[serde(rename = "ParentIDs")]
    pub parent_ids: Vec<LinkId>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeCreationRequest {
    #[serde(rename = "AddressID")]
    pub address_id: AddressId,

    #[serde(rename = "AddressKeyID")]
    pub address_key_id: AddressKeyId,

    pub share_key: PgpArmoredPrivateKey,
    pub share_passphrase: PgpArmoredMessage,
    pub share_passphrase_signature: PgpArmoredSignature,
    pub folder_name: PgpArmoredMessage,
    pub folder_key: PgpArmoredPrivateKey,
    pub folder_passphrase: PgpArmoredMessage,
    pub folder_passphrase_signature: PgpArmoredSignature,
    pub folder_hash_key: PgpArmoredMessage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeCreationResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    pub volume: VolumeDto,
}

impl VolumeCreationResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeDetailsDto {
    #[serde(rename = "ID")]
    pub id: VolumeId,

    pub used_space: i64,
    pub state: VolumeState,
    pub share: VolumeShareDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeDto {
    #[serde(rename = "VolumeID")]
    pub id: VolumeId,

    pub max_space: Option<i64>,
    pub used_space: i64,
    pub state: VolumeState,
    pub r#type: VolumeType,

    #[serde(rename = "Share")]
    pub root: VolumeRootDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    pub volume: VolumeDetailsDto,
}

impl VolumeResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Deserialize)]
pub struct VolumeRootDto {
    #[serde(rename = "ShareID")]
    pub share_id: ShareId,

    #[serde(rename = "LinkID")]
    pub link_id: LinkId,
}

#[derive(Debug, Deserialize)]
pub struct VolumeShareDto {
    #[serde(rename = "ShareID")]
    pub share_id: ShareId,

    #[serde(rename = "LinkID")]
    pub link_id: LinkId,
}

#[derive(Debug, Deserialize)]
pub struct VolumeTrashResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "Trash")]
    pub trash_by_share: Vec<ShareTrashDto>,
}

impl VolumeTrashResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}
