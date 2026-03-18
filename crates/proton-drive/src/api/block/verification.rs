use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use reqwest_middleware::ClientWithMiddleware;
use reqwest::Url;
use proton_sdk_rs2::auth::TokenCredential;
use crate::links::LinkId;
use crate::revision::RevisionId;
use crate::volume::VolumeId;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BlockVerificationInputResponse {
    #[serde(with = "crate::utils::serde::base64_bytes_opt", default)]
    pub verification_code: Option<Vec<u8>>,
    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub content_key_packet: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BlockVerificationOutput {
    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub token: Vec<u8>,
}

#[async_trait]
pub trait BlockVerificationApiClient: Send + Sync {
    async fn get_verification_input(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        revision_id: &RevisionId,
    ) -> anyhow::Result<BlockVerificationInputResponse>;
}

pub struct DefaultBlockVerificationApiClient {
    client: ClientWithMiddleware,
    base_url: Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultBlockVerificationApiClient {
    pub fn new(
        client: ClientWithMiddleware,
        base_url: Url,
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
impl BlockVerificationApiClient for DefaultBlockVerificationApiClient {
    async fn get_verification_input(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        revision_id: &RevisionId,
    ) -> anyhow::Result<BlockVerificationInputResponse> {
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/links/{}/revisions/{}/verification",
            volume_id.raw(),
            link_id.raw(),
            revision_id.raw()
        ))?;

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
            .json::<BlockVerificationInputResponse>()
            .await?;

        Ok(response)
        }
        }