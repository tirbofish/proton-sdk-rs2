use crate::api::ApiResponse;
use crate::links::LinkId;
use crate::volume::VolumeId;
use async_trait::async_trait;
use proton_sdk_rs2::auth::TokenCredential;
use reqwest::Url;
use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;

/// `https://drive-api.proton.me/drive/` → `https://drive-api.proton.me/docs/`
pub fn docs_base_url(drive_base: &Url) -> Url {
    let mut url = drive_base.clone();
    let path = url.path().trim_end_matches('/');
    let new_path = match path.strip_suffix("/drive") {
        Some(prefix) => format!("{prefix}/docs/"),
        None => format!("{path}/docs/"),
    };
    url.set_path(&new_path);
    url
}

#[derive(Debug, Clone, Deserialize)]
pub struct DocumentMetaDto {
    #[serde(rename = "VolumeID")]
    pub volume_id: String,
    #[serde(rename = "LinkID")]
    pub link_id: String,
    #[serde(rename = "CommitIDs")]
    pub commit_ids: Vec<String>,
    #[serde(rename = "CreateTime")]
    pub create_time: i64,
    #[serde(rename = "ModifyTime")]
    pub modify_time: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct GetDocumentMetaResponse {
    #[serde(rename = "Document")]
    document: DocumentMetaDto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecentDocumentDto {
    #[serde(rename = "VolumeID")]
    pub volume_id: String,
    #[serde(rename = "LinkID")]
    pub link_id: String,
    #[serde(rename = "LastOpenTime")]
    pub last_open_time: i64,
    #[serde(rename = "ContextShareID")]
    pub context_share_id: String,
    #[serde(rename = "AncestorIDs")]
    pub ancestor_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GetRecentsResponse {
    #[serde(rename = "RecentDocuments")]
    recent_documents: Vec<RecentDocumentDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ValetTokenDto {
    #[serde(rename = "Token")]
    pub token: String,
    #[serde(rename = "RtsApiUrl")]
    pub rts_api_url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CreateValetTokenResponse {
    #[serde(rename = "ValetToken")]
    valet_token: ValetTokenDto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SeedCommitResponse {
    #[serde(rename = "CommitID")]
    pub commit_id: String,
    #[serde(rename = "VolumeID")]
    pub volume_id: Option<String>,
    #[serde(rename = "LinkID")]
    pub link_id: Option<String>,
}


#[async_trait]
pub trait DocsApiClient: Send + Sync {
    async fn get_meta(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
    ) -> anyhow::Result<DocumentMetaDto>;

    async fn get_commit(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        commit_id: &str,
    ) -> anyhow::Result<Vec<u8>>;

    async fn list_recent(&self) -> anyhow::Result<Vec<RecentDocumentDto>>;

    async fn create_document(&self, volume_id: &VolumeId, link_id: &LinkId) -> anyhow::Result<()>;

    async fn create_realtime_token(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        last_commit_id: Option<&str>,
    ) -> anyhow::Result<ValetTokenDto>;

    async fn get_public_meta(
        &self,
        token: &str,
        link_id: &str,
        session_uid: &str,
        access_token: &str,
    ) -> anyhow::Result<DocumentMetaDto>;

    async fn get_public_commit(
        &self,
        token: &str,
        link_id: &str,
        commit_id: &str,
        session_uid: &str,
        access_token: &str,
    ) -> anyhow::Result<Vec<u8>>;

    async fn seed_initial_commit(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        commit: &[u8],
    ) -> anyhow::Result<SeedCommitResponse>;

    async fn lock_document(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        fetch_commit_id: Option<&str>,
    ) -> anyhow::Result<Vec<u8>>;

    async fn squash_commit(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        commit_id: &str,
        squash: &[u8],
    ) -> anyhow::Result<()>;

    async fn unlock_document(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        lock_id: &str,
    ) -> anyhow::Result<()>;
}


pub struct DefaultDocsApiClient {
    client: ClientWithMiddleware,
    base_url: Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultDocsApiClient {
    pub fn new(
        client: ClientWithMiddleware,
        drive_base_url: Url,
        token_credential: Option<TokenCredential>,
    ) -> Self {
        Self {
            client,
            base_url: docs_base_url(&drive_base_url),
            token_credential,
        }
    }

    fn document_path(&self, volume_id: &VolumeId, link_id: &LinkId) -> String {
        format!(
            "volumes/{}/documents/{}",
            volume_id.raw(),
            link_id.raw()
        )
    }

    async fn add_auth(
        &self,
        mut builder: reqwest_middleware::RequestBuilder,
    ) -> anyhow::Result<reqwest_middleware::RequestBuilder> {
        if let Some(credential) = &self.token_credential {
            let (access_token, _) = credential.get_tokens().await?;
            builder = builder.header("Authorization", format!("Bearer {access_token}"));
            builder = builder.header("x-pm-uid", credential.session_id().raw());
        }
        Ok(builder)
    }

    fn with_public_session(
        &self,
        builder: reqwest_middleware::RequestBuilder,
        session_uid: &str,
        access_token: &str,
    ) -> reqwest_middleware::RequestBuilder {
        builder
            .header("Authorization", format!("Bearer {access_token}"))
            .header("x-pm-uid", session_uid)
    }

    async fn send_bytes(
        &self,
        builder: reqwest_middleware::RequestBuilder,
    ) -> anyhow::Result<Vec<u8>> {
        let resp = builder.send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            if let Ok(api) = serde_json::from_str::<ApiResponse>(&body) {
                if let Some(msg) = &api.error_message {
                    return Err(anyhow::anyhow!("API error {}: {}", api.code.0, msg));
                }
            }
            return Err(anyhow::anyhow!("HTTP {status}: {body}"));
        }
        Ok(bytes.to_vec())
    }
}


async fn parse_json<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> anyhow::Result<T> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        if let Ok(api) = serde_json::from_str::<ApiResponse>(&body) {
            if let Some(msg) = &api.error_message {
                return Err(anyhow::anyhow!("API error {}: {}", api.code.0, msg));
            }
        }
        return Err(anyhow::anyhow!("HTTP {status}: {body}"));
    }
    serde_json::from_str::<T>(&body).map_err(|e| {
        anyhow::anyhow!(
            "JSON parse error: {}. Body: {}",
            e,
            &body[..body.len().min(512)]
        )
    })
}

#[async_trait]
impl DocsApiClient for DefaultDocsApiClient {
    async fn get_meta(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
    ) -> anyhow::Result<DocumentMetaDto> {
        let url = self
            .base_url
            .join(&format!("{}/meta", self.document_path(volume_id, link_id)))?;
        let builder = self.add_auth(self.client.get(url)).await?;
        Ok(parse_json::<GetDocumentMetaResponse>(builder.send().await?)
            .await?
            .document)
    }

    async fn get_commit(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        commit_id: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let url = self.base_url.join(&format!(
            "{}/commits/{commit_id}",
            self.document_path(volume_id, link_id)
        ))?;
        let builder = self.add_auth(self.client.get(url)).await?;
        let resp = builder.send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            if let Ok(api) = serde_json::from_str::<ApiResponse>(&body) {
                if let Some(msg) = &api.error_message {
                    return Err(anyhow::anyhow!("API error {}: {}", api.code.0, msg));
                }
            }
            return Err(anyhow::anyhow!("HTTP {status}: {body}"));
        }
        Ok(bytes.to_vec())
    }

    async fn list_recent(&self) -> anyhow::Result<Vec<RecentDocumentDto>> {
        let url = self.base_url.join("recent")?;
        let builder = self.add_auth(self.client.get(url)).await?;
        Ok(parse_json::<GetRecentsResponse>(builder.send().await?)
            .await?
            .recent_documents)
    }

    async fn create_document(&self, volume_id: &VolumeId, link_id: &LinkId) -> anyhow::Result<()> {
        let url = self
            .base_url
            .join(&self.document_path(volume_id, link_id))?;
        let builder = self
            .add_auth(self.client.post(url).json(&serde_json::json!({})))
            .await?;
        let resp = builder.send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            if let Ok(api) = serde_json::from_str::<ApiResponse>(&body) {
                if api.is_success() {
                    return Ok(());
                }
                if let Some(msg) = &api.error_message {
                    return Err(anyhow::anyhow!("API error {}: {}", api.code.0, msg));
                }
            }
            return Err(anyhow::anyhow!("HTTP {status}: {body}"));
        }
        Ok(())
    }

    async fn create_realtime_token(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        last_commit_id: Option<&str>,
    ) -> anyhow::Result<ValetTokenDto> {
        let url = self
            .base_url
            .join(&format!("{}/tokens", self.document_path(volume_id, link_id)))?;
        let builder = self
            .add_auth(self.client.post(url).json(&serde_json::json!({
                "LastCommitID": last_commit_id,
            })))
            .await?;
        Ok(parse_json::<CreateValetTokenResponse>(builder.send().await?)
            .await?
            .valet_token)
    }

    async fn get_public_meta(
        &self,
        token: &str,
        link_id: &str,
        session_uid: &str,
        access_token: &str,
    ) -> anyhow::Result<DocumentMetaDto> {
        let url = self
            .base_url
            .join(&format!("urls/{token}/documents/{link_id}/meta"))?;
        let builder = self.with_public_session(self.client.get(url), session_uid, access_token);
        Ok(parse_json::<GetDocumentMetaResponse>(builder.send().await?)
            .await?
            .document)
    }

    async fn get_public_commit(
        &self,
        token: &str,
        link_id: &str,
        commit_id: &str,
        session_uid: &str,
        access_token: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let url = self.base_url.join(&format!(
            "urls/{token}/documents/{link_id}/commits/{commit_id}"
        ))?;
        let builder = self.with_public_session(self.client.get(url), session_uid, access_token);
        self.send_bytes(builder).await
    }

    async fn seed_initial_commit(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        commit: &[u8],
    ) -> anyhow::Result<SeedCommitResponse> {
        let url = self.base_url.join(&format!(
            "{}/seed-initial-commit",
            self.document_path(volume_id, link_id)
        ))?;
        let builder = self
            .add_auth(
                self.client
                    .post(url)
                    .header("Content-Type", "application/octet-stream")
                    .body(commit.to_vec()),
            )
            .await?;
        parse_json(builder.send().await?).await
    }

    async fn lock_document(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        fetch_commit_id: Option<&str>,
    ) -> anyhow::Result<Vec<u8>> {
        let url = self
            .base_url
            .join(&format!("{}/lock", self.document_path(volume_id, link_id)))?;
        let builder = self
            .add_auth(self.client.post(url).json(&serde_json::json!({
                "FetchCommitID": fetch_commit_id,
            })))
            .await?;
        self.send_bytes(builder).await
    }

    async fn squash_commit(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        commit_id: &str,
        squash: &[u8],
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "{}/commits/{commit_id}/squash",
            self.document_path(volume_id, link_id)
        ))?;
        let builder = self
            .add_auth(
                self.client
                    .put(url)
                    .header("Content-Type", "application/octet-stream")
                    .body(squash.to_vec()),
            )
            .await?;
        let resp = builder.send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            if let Ok(api) = serde_json::from_str::<ApiResponse>(&body) {
                if api.is_success() {
                    return Ok(());
                }
                if let Some(msg) = &api.error_message {
                    return Err(anyhow::anyhow!("API error {}: {}", api.code.0, msg));
                }
            }
            return Err(anyhow::anyhow!("HTTP {status}: {body}"));
        }
        Ok(())
    }

    async fn unlock_document(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        lock_id: &str,
    ) -> anyhow::Result<()> {
        let url = self
            .base_url
            .join(&format!("{}/unlock", self.document_path(volume_id, link_id)))?;
        let builder = self
            .add_auth(self.client.post(url).json(&serde_json::json!({
                "LockID": lock_id,
            })))
            .await?;
        let _ = builder.send().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]

    fn docs_base_replaces_drive_suffix() {
        let drive = Url::parse("https://drive-api.proton.me/drive/").unwrap();
        assert_eq!(
            docs_base_url(&drive).as_str(),
            "https://drive-api.proton.me/docs/"
        );
    }

    #[test]
    fn docs_base_keeps_api_prefix() {
        let drive = Url::parse("https://mail.proton.me/api/drive/").unwrap();
        assert_eq!(
            docs_base_url(&drive).as_str(),
            "https://mail.proton.me/api/docs/"
        );
    }
}
