use crate::api::ApiResponse;
use crate::links::LinkId;
use crate::volume::VolumeId;
use async_trait::async_trait;
use proton_sdk_rs2::auth::TokenCredential;
use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;

#[async_trait]
pub trait EventsApiClient: Send + Sync {
    async fn get_core_latest_event_id(&self) -> anyhow::Result<String>;
    async fn get_core_events(&self, event_id: &str) -> anyhow::Result<CoreEventsResponse>;

    async fn get_volume_latest_event_id(&self, volume_id: VolumeId) -> anyhow::Result<String>;
    async fn get_volume_events(
        &self,
        volume_id: VolumeId,
        event_id: &str,
    ) -> anyhow::Result<VolumeEventsResponse>;
}

pub struct DefaultEventsApiClient {
    client: ClientWithMiddleware,
    base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultEventsApiClient {
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
impl EventsApiClient for DefaultEventsApiClient {
    async fn get_core_latest_event_id(&self) -> anyhow::Result<String> {
        let url = self.base_url.join("/core/v4/events/latest")?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;
        let resp: CoreLatestEventResponse = builder.send().await?.json().await?;
        resp.base.to_result()?;
        Ok(resp.event_id)
    }

    async fn get_core_events(&self, event_id: &str) -> anyhow::Result<CoreEventsResponse> {
        let url = self.base_url.join(&format!("/core/v5/events/{}", event_id))?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;
        let resp: CoreEventsResponse = builder.send().await?.json().await?;
        resp.base.to_result()?;
        Ok(resp)
    }

    async fn get_volume_latest_event_id(&self, volume_id: VolumeId) -> anyhow::Result<String> {
        let url = self.base_url.join(&format!("volumes/{}/events/latest", volume_id.raw()))?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;
        let resp: VolumeLatestEventResponse = builder.send().await?.json().await?;
        resp.base.to_result()?;
        Ok(resp.event_id)
    }

    async fn get_volume_events(
        &self,
        volume_id: VolumeId,
        event_id: &str,
    ) -> anyhow::Result<VolumeEventsResponse> {
        let url = self.base_url.join(&format!("v2/volumes/{}/events/{}", volume_id.raw(), event_id))?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;
        let resp: VolumeEventsResponse = builder.send().await?.json().await?;
        resp.base.to_result()?;
        Ok(resp)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CoreLatestEventResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "EventID")]
    pub event_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CoreEventsResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "EventID")]
    pub event_id: String,
    pub refresh: i32,
    pub more: i32,
    pub drive_share_refresh: Option<DriveShareRefresh>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DriveShareRefresh {
    pub action: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VolumeLatestEventResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "EventID")]
    pub event_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeEventsResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "EventID")]
    pub event_id: String,
    pub more: bool,
    pub refresh: bool,
    pub events: Vec<VolumeEventDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeEventDto {
    #[serde(rename = "EventID")]
    pub event_id: String,
    pub event_type: i32,
    pub link: VolumeEventLinkDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeEventLinkDto {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,
    #[serde(rename = "ParentLinkID")]
    pub parent_link_id: Option<LinkId>,
    pub is_shared: bool,
    pub is_trashed: bool,
}
