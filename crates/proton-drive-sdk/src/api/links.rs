use crate::api::file::FileDto;
use crate::api::file::photos::PhotoDto;
use crate::api::folder::FolderDto;
use crate::api::node::{
    NodeNameAvailabilityRequest, NodeNameAvailabilityResponse, RecentlyAccessedItem,
    RecentlyAccessedRequest,
};
use crate::api::share::{ContextShareResponse, ShareMembershipSummaryDto};
use crate::api::{AggregateApiResponse, ApiResponse};
use crate::links::LinkId;
use crate::pgp::{PgpArmoredMessage, PgpArmoredPrivateKey, PgpArmoredSignature};
use crate::share::ShareId;
use crate::volume::VolumeId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

#[async_trait]
pub trait LinksApiClient: Send + Sync {
    async fn report_recently_accessed(&self, items: &[RecentlyAccessedItem]) -> anyhow::Result<()>;

    async fn get_details(
        &self,
        volume_id: VolumeId,
        link_ids: Vec<LinkId>,
    ) -> anyhow::Result<LinkDetailsResponse>;

    async fn get_context_share(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
    ) -> anyhow::Result<ContextShareResponse>;

    async fn move_link(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: MoveSingleLinkRequest,
    ) -> anyhow::Result<ApiResponse>;

    async fn move_multiple(
        &self,
        volume_id: VolumeId,
        request: MoveMultipleLinksRequest,
    ) -> anyhow::Result<ApiResponse>;

    async fn rename(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: RenameLinkRequest,
    ) -> anyhow::Result<ApiResponse>;

    async fn delete_multiple(
        &self,
        volume_id: VolumeId,
        link_ids: Vec<LinkId>,
    ) -> anyhow::Result<AggregateApiResponse<LinkIdResponsePair>>;

    async fn get_available_names(
        &self,
        volume_id: VolumeId,
        folder_id: LinkId,
        request: NodeNameAvailabilityRequest,
    ) -> anyhow::Result<NodeNameAvailabilityResponse>;

    async fn copy_link(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: CopyLinkRequest,
    ) -> anyhow::Result<CopyLinkResponse>;
}

use proton_sdk_rs2::auth::TokenCredential;

pub struct DefaultLinksApiClient {
    client: ClientWithMiddleware,
    base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultLinksApiClient {
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

    pub(crate) async fn report_recently_accessed_to(
        &self,
        path: &str,
        items: &[RecentlyAccessedItem],
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(path)?;
        let now = Utc::now();
        for batch in items.chunks(50) {
            let builder = self
                .add_auth_headers(
                    self.client
                        .post(url.clone())
                        .json(&RecentlyAccessedRequest::new(batch, now)),
                )
                .await?;
            let response = builder.send().await?;
            ApiResponse::from_response(response).await?.to_result()?;
        }
        Ok(())
    }
}

#[async_trait]
impl LinksApiClient for DefaultLinksApiClient {
    async fn report_recently_accessed(&self, items: &[RecentlyAccessedItem]) -> anyhow::Result<()> {
        self.report_recently_accessed_to("recently-accessed-items", items)
            .await
    }

    async fn get_details(
        &self,
        volume_id: VolumeId,
        link_ids: Vec<LinkId>,
    ) -> anyhow::Result<LinkDetailsResponse> {
        let url = self
            .base_url
            .join(&format!("v2/volumes/{}/links", volume_id.raw()))?;
        let builder = self
            .client
            .post(url)
            .json(&LinkDetailsRequest::new(link_ids));
        let builder = self.add_auth_headers(builder).await?;
        let response = builder.send().await?;
        let text = response.text().await?;
        let result = match serde_json::from_str::<LinkDetailsResponse>(&text) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "LinkDetailsResponse failed to deserialize");
                tracing::debug!(body = %text, "Full response");
                return Err(e.into());
            }
        };
        result.base.to_result()?;
        Ok(result)
    }

    async fn get_context_share(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
    ) -> anyhow::Result<ContextShareResponse> {
        let url = self.base_url.join(&format!(
            "volumes/{}/links/{}/context",
            volume_id.raw(),
            link_id.raw()
        ))?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder.send().await?.json::<ContextShareResponse>().await?)
    }

    async fn move_link(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: MoveSingleLinkRequest,
    ) -> anyhow::Result<ApiResponse> {
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/links/{}/move",
            volume_id.raw(),
            link_id.raw()
        ))?;

        let request_json = serde_json::to_string(&request)?;
        tracing::debug!(url = %url, body = %request_json, "Sending move_link request");

        let builder = self.client.put(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        let response = builder.send().await?;
        let text = response.text().await?;
        tracing::debug!(body = %text, "move_link response received");

        let api_response = serde_json::from_str::<ApiResponse>(&text)?;
        api_response.to_result()?;
        Ok(api_response)
    }

    async fn move_multiple(
        &self,
        volume_id: VolumeId,
        request: MoveMultipleLinksRequest,
    ) -> anyhow::Result<ApiResponse> {
        let url = self
            .base_url
            .join(&format!("volumes/{}/links/move-multiple", volume_id.raw()))?;

        let request_json = serde_json::to_string(&request)?;
        tracing::debug!(url = %url, body = %request_json, "Sending move_multiple request");

        let builder = self.client.put(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        let response = builder.send().await?;
        let text = response.text().await?;
        tracing::debug!(body = %text, "move_multiple response received");

        let api_response = serde_json::from_str::<ApiResponse>(&text)?;
        api_response.to_result()?;
        Ok(api_response)
    }

    async fn rename(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: RenameLinkRequest,
    ) -> anyhow::Result<ApiResponse> {
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/links/{}/rename",
            volume_id.raw(),
            link_id.raw()
        ))?;
        let builder = self.client.put(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder.send().await?.json::<ApiResponse>().await?)
    }

    async fn delete_multiple(
        &self,
        volume_id: VolumeId,
        link_ids: Vec<LinkId>,
    ) -> anyhow::Result<AggregateApiResponse<LinkIdResponsePair>> {
        let url = self
            .base_url
            .join(&format!("v2/volumes/{}/delete_multiple", volume_id.raw()))?;
        let builder = self
            .client
            .post(url)
            .json(&MultipleLinksNullaryRequest { link_ids });
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder
            .send()
            .await?
            .json::<AggregateApiResponse<LinkIdResponsePair>>()
            .await?)
    }

    async fn get_available_names(
        &self,
        volume_id: VolumeId,
        folder_id: LinkId,
        request: NodeNameAvailabilityRequest,
    ) -> anyhow::Result<NodeNameAvailabilityResponse> {
        let url = self.base_url.join(&format!(
            "v2/volumes/{}/links/{}/checkAvailableHashes",
            volume_id.raw(),
            folder_id.raw()
        ))?;
        let builder = self.client.post(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder
            .send()
            .await?
            .json::<NodeNameAvailabilityResponse>()
            .await?)
    }

    async fn copy_link(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: CopyLinkRequest,
    ) -> anyhow::Result<CopyLinkResponse> {
        let url = self.base_url.join(&format!(
            "volumes/{}/links/{}/copy",
            volume_id.raw(),
            link_id.raw()
        ))?;
        let builder = self.client.post(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        let response = builder.send().await?;
        let text = response.text().await?;
        tracing::debug!(body = %text, "copy_link response received");
        let api_response = serde_json::from_str::<crate::api::ApiResponse>(&text)?;
        api_response.to_result()?;
        Ok(serde_json::from_str::<CopyLinkResponse>(&text)?)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LinkDetailsDto {
    #[serde(rename = "Link")]
    pub link: LinkDto,
    #[serde(rename = "Folder")]
    pub folder: Option<FolderDto>,
    #[serde(rename = "File")]
    pub file: Option<FileDto>,
    #[serde(rename = "Photo")]
    pub photo: Option<PhotoDto>,
    #[serde(rename = "Album")]
    pub album: Option<FolderDto>,
    #[serde(rename = "Sharing")]
    pub sharing: Option<LinkSharingDto>,
    #[serde(rename = "Membership")]
    pub membership: Option<ShareMembershipSummaryDto>,
}

impl LinkDetailsDto {
    pub fn deconstruct(
        self,
    ) -> (
        LinkDto,
        Option<FolderDto>,
        Option<FileDto>,
        Option<PhotoDto>,
        Option<FolderDto>,
        Option<LinkSharingDto>,
        Option<ShareMembershipSummaryDto>,
    ) {
        (
            self.link,
            self.folder,
            self.file,
            self.photo,
            self.album,
            self.sharing,
            self.membership,
        )
    }
}

#[derive(Debug, Serialize)]
pub struct LinkDetailsRequest {
    #[serde(rename = "LinkIDs")]
    pub link_ids: Vec<LinkId>,
}

impl LinkDetailsRequest {
    pub fn new(link_ids: Vec<LinkId>) -> Self {
        Self { link_ids }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LinkDetailsResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(default)]
    pub links: Vec<LinkDetailsDto>,
}

impl LinkDetailsResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum LinkType {
    Folder = 1,
    File = 2,
    Album = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum LinkState {
    /// File is created, waiting for the revision to be committed.
    /// Automatically garbage collected if no blocks uploaded within last 3 hours.
    Draft = 0,
    /// Active
    Active = 1,
    /// Trashed
    Trashed = 2,
    /// Permanently deleted, waiting for garbage collection.
    /// Should not appear in API responses.
    Deleted = 3,
    /// Hidden, being restored from old volume.
    /// Should not appear in API responses.
    Restoring = 4,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OwnedByDto {
    #[serde(rename = "Email")]
    pub email: Option<String>,

    #[serde(rename = "Organization")]
    pub organization: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LinkDto {
    #[serde(rename = "LinkID")]
    pub id: LinkId,

    #[serde(rename = "Type")]
    pub r#type: LinkType,

    #[serde(rename = "ParentLinkID")]
    pub parent_id: Option<LinkId>,

    #[serde(rename = "State")]
    pub state: LinkState,

    #[serde(rename = "CreateTime")]
    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub creation_time: DateTime<Utc>,

    #[serde(rename = "ModifyTime")]
    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub modification_time: DateTime<Utc>,

    #[serde(rename = "Trashed")]
    #[serde(default, with = "crate::utils::serde::epoch_seconds_opt")]
    pub trash_time: Option<DateTime<Utc>>,

    #[serde(rename = "Name")]
    pub name: PgpArmoredMessage,

    #[serde(rename = "NameHash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub name_hash_digest: Vec<u8>,

    #[serde(rename = "NodeKey")]
    pub key: PgpArmoredPrivateKey,

    #[serde(rename = "NodePassphrase")]
    pub passphrase: PgpArmoredMessage,

    #[serde(rename = "NodePassphraseSignature")]
    pub passphrase_signature: Option<PgpArmoredSignature>,

    #[serde(rename = "SignatureEmail")]
    pub signature_email_address: Option<String>,

    #[serde(rename = "NameSignatureEmail")]
    pub name_signature_email_address: Option<String>,

    #[serde(rename = "MIMEType")]
    pub media_type: Option<String>,

    #[serde(rename = "OwnedBy")]
    pub owned_by: Option<OwnedByDto>,
}

#[derive(Debug, Deserialize)]
pub struct LinkIdResponsePair {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    #[serde(rename = "Response")]
    pub response: ApiResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LinkSharingDto {
    #[serde(rename = "ShareID")]
    pub share_id: ShareId,

    #[serde(rename = "ShareURLID")]
    pub share_url_id: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct MoveMultipleLinksItem {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    #[serde(rename = "Name")]
    pub name: PgpArmoredMessage,

    #[serde(rename = "NodePassphrase")]
    pub passphrase: PgpArmoredMessage,

    #[serde(rename = "Hash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub name_hash_digest: Vec<u8>,

    #[serde(rename = "OriginalHash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub original_name_hash_digest: Vec<u8>,

    #[serde(rename = "NodePassphraseSignature")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase_signature: Option<PgpArmoredSignature>,
}

#[derive(Debug, Serialize, Clone)]
pub struct MoveMultipleLinksRequest {
    #[serde(rename = "ParentLinkID")]
    pub parent_link_id: LinkId,

    #[serde(rename = "Links")]
    pub batch: Vec<MoveMultipleLinksItem>,

    #[serde(rename = "NameSignatureEmail")]
    pub name_signature_email_address: String,

    #[serde(rename = "SignatureEmail")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NameHashDigestUnavailabilityDto {
    #[serde(rename = "Hash")]
    pub name_hash_digest: String,

    #[serde(rename = "RevisionID")]
    pub revision_id: crate::revision::RevisionId,

    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    #[serde(rename = "ClientUID")]
    pub client_uid: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct MoveSingleLinkRequest {
    #[serde(rename = "Name")]
    pub name: PgpArmoredMessage,

    #[serde(rename = "NodePassphrase")]
    pub passphrase: PgpArmoredMessage,

    #[serde(rename = "Hash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub name_hash_digest: Vec<u8>,

    #[serde(rename = "ParentLinkID")]
    pub parent_link_id: LinkId,

    #[serde(rename = "OriginalHash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub original_name_hash_digest: Vec<u8>,

    #[serde(rename = "NameSignatureEmail")]
    pub name_signature_email_address: String,

    #[serde(rename = "ContentHash")]
    pub content_hash: Option<String>,

    #[serde(rename = "NodePassphraseSignature")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase_signature: Option<PgpArmoredSignature>,

    #[serde(rename = "SignatureEmail")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_email_address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MultipleLinksNullaryRequest {
    #[serde(rename = "LinkIDs")]
    pub link_ids: Vec<LinkId>,
}

#[derive(Debug, Serialize, Clone)]
pub struct CopyLinkRequest {
    #[serde(rename = "TargetVolumeID")]
    pub target_volume_id: VolumeId,

    #[serde(rename = "TargetParentLinkID")]
    pub target_parent_link_id: LinkId,

    #[serde(rename = "Name")]
    pub name: PgpArmoredMessage,

    #[serde(rename = "NodePassphrase")]
    pub passphrase: PgpArmoredMessage,

    #[serde(rename = "Hash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub name_hash_digest: Vec<u8>,

    #[serde(rename = "NameSignatureEmail")]
    pub name_signature_email_address: String,

    #[serde(rename = "NodePassphraseSignature")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase_signature: Option<PgpArmoredSignature>,

    #[serde(rename = "SignatureEmail")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CopyLinkResponse {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,
}

#[derive(Debug, Serialize)]
pub struct RenameLinkRequest {
    #[serde(rename = "Name")]
    pub name: PgpArmoredMessage,

    #[serde(rename = "Hash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub name_hash_digest: Vec<u8>,

    #[serde(rename = "NameSignatureEmail")]
    pub name_signature_email_address: String,

    #[serde(rename = "MIMEType")]
    pub media_type: Option<String>,

    #[serde(rename = "OriginalHash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub original_name_hash_digest: Vec<u8>,
}

#[cfg(test)]
mod recently_accessed_tests {
    use super::*;
    use crate::api::file::photos::PhotosLinksApiClient;
    use crate::node::NodeUid;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn receive_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(count > 0, "request ended before body");
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse().ok())
                    })
                    .unwrap();
                if bytes.len() >= header_end + 4 + length {
                    return String::from_utf8(bytes).unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn recently_accessed_batches_and_routes_drive_and_photos() {
        for (photos, path) in [
            (false, "/drive/recently-accessed-items"),
            (true, "/drive/photos/recently-accessed-items"),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url =
                reqwest::Url::parse(&format!("http://{}/drive/", listener.local_addr().unwrap()))
                    .unwrap();
            let http = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
            let client: Box<dyn LinksApiClient> = if photos {
                Box::new(PhotosLinksApiClient::new(http, url, None))
            } else {
                Box::new(DefaultLinksApiClient::new(http, url, None))
            };
            let items: Vec<_> = (0..51)
                .map(|index| RecentlyAccessedItem {
                    node_uid: NodeUid::from_parts("volume", format!("link{index}")),
                    access_time: Some(
                        DateTime::<Utc>::from_timestamp(1_700_000_000, 500_000_000).unwrap(),
                    ),
                })
                .collect();

            let server = tokio::spawn(async move {
                let mut requests = Vec::new();
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    requests.push(receive_request(&mut stream).await);
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\n{\"Code\":1000}")
                        .await
                        .unwrap();
                }
                requests
            });
            client.report_recently_accessed(&[]).await.unwrap();
            client.report_recently_accessed(&items).await.unwrap();
            let requests = server.await.unwrap();
            for (index, request) in requests.iter().enumerate() {
                assert!(request.starts_with(&format!("POST {path} HTTP/1.1")));
                let body: serde_json::Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                let sent = body["RecentlyAccessedItems"].as_array().unwrap();
                assert_eq!(sent.len(), if index == 0 { 50 } else { 1 });
                assert_eq!(
                    sent[0],
                    serde_json::json!({
                        "VolumeID": "volume",
                        "LinkID": format!("link{}", index * 50),
                        "AccessTime": 1_700_000_000,
                    })
                );
            }
        }
    }

    #[tokio::test]
    async fn recently_accessed_stops_after_api_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = reqwest::Url::parse(&format!("http://{}/drive/", listener.local_addr().unwrap()))
            .unwrap();
        let client = DefaultLinksApiClient::new(
            reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build(),
            url,
            None,
        );
        let items: Vec<_> = (0..101)
            .map(|index| RecentlyAccessedItem {
                node_uid: NodeUid::from_parts("volume", format!("link{index}")),
                access_time: None,
            })
            .collect();
        let server = tokio::spawn(async move {
            for response in [r#"{"Code":1000}"#, r#"{"Code":2500,"Error":"failure"}"#] {
                let (mut stream, _) = listener.accept().await.unwrap();
                receive_request(&mut stream).await;
                stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                                response.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        assert!(
            client
                .report_recently_accessed(&items)
                .await
                .unwrap_err()
                .to_string()
                .contains("failure")
        );
        server.await.unwrap();
    }
}
