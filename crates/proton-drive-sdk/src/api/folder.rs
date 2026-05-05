use crate::api::ApiResponse;
use crate::api::node::NodeCreationRequest;
use crate::links::LinkId;
use crate::pgp::PgpArmoredMessage;
use crate::volume::VolumeId;
use async_trait::async_trait;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct FolderChildrenResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "LinkIDs", default)]
    pub link_ids: Vec<LinkId>,

    pub anchor_id: Option<LinkId>,

    #[serde(rename = "More", default)]
    pub more_results_exist: bool,
}

impl FolderChildrenResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Serialize)]
pub struct FolderCreationRequest {
    #[serde(flatten)]
    pub base: NodeCreationRequest,

    #[serde(rename = "NodeHashKey")]
    pub hash_key: PgpArmoredMessage,

    #[serde(rename = "SignatureEmail")]
    pub signature_email_address: String,

    #[serde(rename = "XAttr")]
    pub extended_attributes: Option<PgpArmoredMessage>,
}

#[derive(Debug, Deserialize)]
pub struct FolderCreationResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "Folder")]
    pub folder_id: FolderId,
}

impl FolderCreationResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FolderDto {
    #[serde(rename = "NodeHashKey")]
    pub hash_key: Option<PgpArmoredMessage>,

    #[serde(rename = "XAttr")]
    pub extended_attributes: Option<PgpArmoredMessage>,
}

#[derive(Debug, Deserialize)]
pub struct FolderId {
    #[serde(rename = "ID")]
    pub value: LinkId,
}

#[async_trait]
pub trait FoldersApiClient: Send + Sync {
    async fn get_children(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        anchor_id: Option<LinkId>,
    ) -> anyhow::Result<FolderChildrenResponse>;

    async fn create_folder(
        &self,
        volume_id: VolumeId,
        request: FolderCreationRequest,
    ) -> anyhow::Result<FolderCreationResponse>;
}

use proton_sdk_rs2::auth::TokenCredential;

pub struct DefaultFoldersApiClient {
    client: ClientWithMiddleware,
    base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultFoldersApiClient {
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
impl FoldersApiClient for DefaultFoldersApiClient {
    async fn get_children(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        anchor_id: Option<LinkId>,
    ) -> anyhow::Result<FolderChildrenResponse> {
        let path = match anchor_id {
            Some(anchor) => format!(
                "v2/volumes/{}/folders/{}/children?AnchorID={}",
                volume_id.raw(),
                link_id.raw(),
                anchor.raw()
            ),
            None => format!(
                "v2/volumes/{}/folders/{}/children",
                volume_id.raw(),
                link_id.raw()
            ),
        };
        let url = self.base_url.join(&path)?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;

        let resp: FolderChildrenResponse = builder.send().await?.json().await?;
        resp.base.to_result()?;
        Ok(resp)
    }

    async fn create_folder(
        &self,
        volume_id: VolumeId,
        request: FolderCreationRequest,
    ) -> anyhow::Result<FolderCreationResponse> {
        let url = self
            .base_url
            .join(&format!("v2/volumes/{}/folders", volume_id.raw()))?;
        let builder = self.client.post(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        let response = builder.send().await?;

        let text = response.text().await?;
        let res: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!("Failed to decode response body: {}. Body: {}", e, text)
        })?;

        if let Some(code) = res.get("Code").and_then(|c| c.as_u64()) {
            if code != 1000 {
                let error = res
                    .get("Error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("Unknown error");
                anyhow::bail!("API error: code {}, message: {}", code, error);
            }
        }

        let folder_creation_response: FolderCreationResponse = serde_json::from_value(res)?;
        Ok(folder_creation_response)
    }
}
