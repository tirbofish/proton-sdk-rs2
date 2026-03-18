use crate::api::AggregateApiResponse;
use crate::api::ApiResponse;
use crate::api::links::{LinkIdResponsePair, MultipleLinksNullaryRequest};
use crate::api::volumes::VolumeTrashResponse;
use crate::volume::VolumeId;
use async_trait::async_trait;
use reqwest_middleware::ClientWithMiddleware;

#[async_trait]
pub trait TrashApiClient: Send + Sync {
    async fn get_trash(
        &self,
        volume_id: VolumeId,
        page_size: i32,
        page: i32,
    ) -> anyhow::Result<VolumeTrashResponse>;

    async fn trash_multiple(
        &self,
        volume_id: VolumeId,
        request: MultipleLinksNullaryRequest,
    ) -> anyhow::Result<AggregateApiResponse<LinkIdResponsePair>>;

    async fn restore_multiple(
        &self,
        volume_id: VolumeId,
        request: MultipleLinksNullaryRequest,
    ) -> anyhow::Result<AggregateApiResponse<LinkIdResponsePair>>;

    async fn delete_multiple(
        &self,
        volume_id: VolumeId,
        request: MultipleLinksNullaryRequest,
    ) -> anyhow::Result<AggregateApiResponse<LinkIdResponsePair>>;

    async fn empty(&self, volume_id: VolumeId) -> anyhow::Result<ApiResponse>;
}

use proton_sdk_rs2::auth::TokenCredential;

pub struct DefaultTrashApiClient {
    client: ClientWithMiddleware,
    base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultTrashApiClient {
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
impl TrashApiClient for DefaultTrashApiClient {
    async fn get_trash(
        &self,
        volume_id: VolumeId,
        page_size: i32,
        page: i32,
    ) -> anyhow::Result<VolumeTrashResponse> {
        let url = self.base_url.join(&format!(
            "volumes/{}/trash?pageSize={}&page={}",
            volume_id.raw(),
            page_size,
            page
        ))?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder.send().await?.json::<VolumeTrashResponse>().await?)
    }

    async fn trash_multiple(
        &self,
        volume_id: VolumeId,
        request: MultipleLinksNullaryRequest,
    ) -> anyhow::Result<AggregateApiResponse<LinkIdResponsePair>> {
        let url = self
            .base_url
            .join(&format!("v2/volumes/{}/trash_multiple", volume_id.raw()))?;
        let builder = self.client.post(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        let response = builder.send().await?;
        let text = response.text().await?;
        let result = match serde_json::from_str::<AggregateApiResponse<LinkIdResponsePair>>(&text) {
            Ok(r) => r,
            Err(e) => {
                println!("DEBUG: trash_multiple failed to deserialize: {}", e);
                println!("DEBUG: Full response: {}", text);
                return Err(e.into());
            }
        };
        Ok(result)
    }

    async fn restore_multiple(
        &self,
        volume_id: VolumeId,
        request: MultipleLinksNullaryRequest,
    ) -> anyhow::Result<AggregateApiResponse<LinkIdResponsePair>> {
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/trash/restore_multiple",
            volume_id.raw()
        ))?;
        let builder = self.client.put(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder
            .send()
            .await?
            .json::<AggregateApiResponse<LinkIdResponsePair>>()
            .await?)
    }

    async fn delete_multiple(
        &self,
        volume_id: VolumeId,
        request: MultipleLinksNullaryRequest,
    ) -> anyhow::Result<AggregateApiResponse<LinkIdResponsePair>> {
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/trash/delete_multiple",
            volume_id.raw()
        ))?;
        let builder = self.client.post(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder
            .send()
            .await?
            .json::<AggregateApiResponse<LinkIdResponsePair>>()
            .await?)
    }

    async fn empty(&self, volume_id: VolumeId) -> anyhow::Result<ApiResponse> {
        let url = self
            .base_url
            .join(&format!("volumes/{}/trash", volume_id.raw()))?;
        let builder = self.client.delete(url);
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder.send().await?.json::<ApiResponse>().await?)
    }
}
