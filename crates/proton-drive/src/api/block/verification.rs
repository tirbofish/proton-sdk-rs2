use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use crate::links::LinkId;
use crate::revision::RevisionId;
use crate::volume::VolumeId;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BlockVerificationInputResponse {
    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub verification_code: Vec<u8>,
    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub content_key_packet: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BlockVerificationOutput {
    #[serde(with = "crate::utils::serde::base64_bytes")]
    token: Vec<u8>,
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
    client: reqwest::Client,
    base_url: String,
}

impl DefaultBlockVerificationApiClient {
    pub fn new(client: reqwest::Client, base_url: String) -> Self {
        Self { client, base_url }
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
        let url = format!(
            "{}/v2/volumes/{}/links/{}/revisions/{}/verification",
            self.base_url,
            volume_id.raw(),
            link_id.raw(),
            revision_id.raw()
        );

        let response = self.client
            .get(&url)
            .send()
            .await?
            .json::<BlockVerificationInputResponse>()
            .await?;

        Ok(response)
    }
}