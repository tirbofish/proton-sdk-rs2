use crate::account::{AddressId, AddressKeyId};
use crate::api::devices::DevicesApiClient;
use crate::api::events::{DefaultEventsApiClient, EventsApiClient};
use crate::api::file::{DefaultFilesApiClient, FileDto, FilesApiClient};
use crate::api::folder::{DefaultFoldersApiClient, FoldersApiClient};
use crate::api::links::{
    CopyLinkRequest, CopyLinkResponse, DefaultLinksApiClient, LinkDetailsDto, LinkDetailsRequest,
    LinkDetailsResponse, LinkIdResponsePair, LinksApiClient, MoveMultipleLinksRequest,
    MoveSingleLinkRequest, RenameLinkRequest,
};
use crate::api::node::{NodeNameAvailabilityRequest, NodeNameAvailabilityResponse};
use crate::api::share::{
    ContextShareResponse, DefaultSharesApiClient, ShareResponseV2, SharesApiClient,
};
use crate::api::storage::{DefaultStorageApiClient, StorageApiClient};
use crate::api::trash::{DefaultTrashApiClient, TrashApiClient};
use crate::api::volumes::{DefaultVolumesApiClient, VolumeCreationResponse, VolumesApiClient};
use crate::api::{AggregateApiResponse, ApiResponse, DriveApiClients};
use crate::links::LinkId;
use crate::node::photo::PhotoTag;
use crate::node::NodeUid;
use crate::pgp::{PgpArmoredMessage, PgpArmoredPrivateKey, PgpArmoredSignature};
use crate::volume::VolumeId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use proton_sdk_rs2::auth::TokenCredential;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

#[async_trait]
pub trait PhotosApiClient: Send + Sync {
    async fn create_volume(
        &self,
        request: PhotosVolumeCreationRequest,
    ) -> anyhow::Result<VolumeCreationResponse>;

    async fn get_root_share(&self) -> anyhow::Result<ShareResponseV2>;

    async fn get_timeline_photos(
        &self,
        request: TimelinePhotoListRequest,
    ) -> anyhow::Result<TimelinePhotoListResponse>;

    async fn check_duplicates(
        &self,
        volume_id: VolumeId,
        name_hashes: Vec<String>,
    ) -> anyhow::Result<CheckDuplicatesResponse>;

    async fn get_albums(
        &self,
        volume_id: VolumeId,
        anchor_id: Option<LinkId>,
    ) -> anyhow::Result<AlbumsListResponse>;

    async fn get_album_children(
        &self,
        volume_id: VolumeId,
        album_link_id: LinkId,
        anchor_id: Option<LinkId>,
    ) -> anyhow::Result<AlbumChildrenResponse>;

    async fn create_album(
        &self,
        volume_id: VolumeId,
        request: AlbumCreationRequest,
    ) -> anyhow::Result<AlbumCreationResponse>;

    async fn update_album(
        &self,
        volume_id: VolumeId,
        album_link_id: LinkId,
        request: AlbumUpdateRequest,
    ) -> anyhow::Result<()>;

    async fn delete_album(
        &self,
        volume_id: VolumeId,
        album_link_id: LinkId,
        force: bool,
    ) -> anyhow::Result<()>;

    async fn add_photos_to_album(
        &self,
        volume_id: VolumeId,
        album_link_id: LinkId,
        request: AddPhotosToAlbumRequest,
    ) -> anyhow::Result<AddPhotosToAlbumResponse>;

    async fn remove_photos_from_album(
        &self,
        volume_id: VolumeId,
        album_link_id: LinkId,
        link_ids: Vec<LinkId>,
    ) -> anyhow::Result<()>;

    async fn add_photo_tags(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        tags: Vec<PhotoTag>,
    ) -> anyhow::Result<()>;

    async fn remove_photo_tags(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        tags: Vec<PhotoTag>,
    ) -> anyhow::Result<()>;

    async fn set_photo_favorite(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        payload: Option<FavoritePhotoPayload>,
    ) -> anyhow::Result<()>;
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PhotosAttributesDto {
    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub capture_time: chrono::DateTime<chrono::Utc>,

    #[serde(rename = "ContentHash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub content_hash_digest: Vec<u8>,

    #[serde(rename = "MainPhotoLinkID")]
    pub main_photo_link_id: Option<LinkId>,

    pub tags: Option<HashSet<PhotoTag>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PhotosVolumeCreationRequest {
    pub share: PhotosVolumeShareCreationParameters,
    pub link: PhotosVolumeLinkCreationParameters,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PhotosVolumeLinkCreationParameters {
    pub name: PgpArmoredMessage,
    pub node_key: PgpArmoredPrivateKey,
    pub node_passphrase: PgpArmoredMessage,
    pub node_passphrase_signature: PgpArmoredSignature,
    pub node_hash_key: PgpArmoredMessage,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PhotosVolumeShareCreationParameters {
    #[serde(rename = "AddressID")]
    pub address_id: AddressId,

    #[serde(rename = "AddressKeyID")]
    pub address_key_id: AddressKeyId,

    pub key: PgpArmoredPrivateKey,
    pub passphrase: PgpArmoredMessage,
    pub passphrase_signature: PgpArmoredSignature,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RelatedPhotoDto {
    #[serde(rename = "LinkID")]
    pub id: LinkId,

    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub capture_time: DateTime<Utc>,

    #[serde(rename = "Hash")]
    pub name_hash: String,

    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TimelinePhotoDto {
    #[serde(rename = "LinkID")]
    pub id: LinkId,

    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub capture_time: DateTime<Utc>,

    #[serde(rename = "Hash")]
    pub name_hash: String,

    pub content_hash: Option<String>,

    #[serde(default)]
    pub related_photos: Vec<RelatedPhotoDto>,

    #[serde(default)]
    pub tags: Vec<PhotoTag>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct TimelinePhotoListRequest {
    pub volume_id: VolumeId,

    #[serde(rename = "PreviousPageLastLinkID")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_page_last_link_id: Option<LinkId>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TimelinePhotoListResponse {
    pub photos: Vec<TimelinePhotoDto>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PhotoAlbumInclusionDto {
    #[serde(rename = "AlbumLinkID")]
    pub id: LinkId,

    #[serde(rename = "Hash")]
    pub name_hash: String,

    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub content_hash: Vec<u8>,

    #[serde(rename = "AddedTime")]
    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub creation_time: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PhotoDto {
    #[serde(flatten)]
    pub base: FileDto,

    #[serde(rename = "LinkID")]
    pub id: Option<LinkId>,

    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub capture_time: DateTime<Utc>,

    #[serde(default, with = "crate::utils::serde::forgiving_hex_bytes_opt")]
    pub content_hash: Option<Vec<u8>>,

    #[serde(rename = "Hash")]
    pub name_hash: Option<String>,

    #[serde(rename = "MainPhotoLinkID")]
    pub main_photo_link_id: Option<String>,

    #[serde(rename = "RelatedPhotosLinkIDs", default)]
    pub related_photos_link_ids: Vec<String>,

    #[serde(default)]
    pub tags: Vec<PhotoTag>,

    #[serde(rename = "Albums", default)]
    pub album_inclusions: Vec<PhotoAlbumInclusionDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PhotoDetailsResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    pub links: Vec<LinkDetailsDto>,
}

impl PhotoDetailsResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CheckDuplicatesRequest {
    pub name_hashes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DuplicateHashDto {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    #[serde(rename = "Hash")]
    pub name_hash: String,

    pub content_hash: Option<String>,

    pub link_state: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CheckDuplicatesResponse {
    pub duplicate_hashes: Vec<DuplicateHashDto>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumListItemDto {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    pub photo_count: u64,

    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub last_activity_time: DateTime<Utc>,

    #[serde(rename = "CoverLinkID")]
    pub cover_link_id: Option<LinkId>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumsListResponse {
    pub albums: Vec<AlbumListItemDto>,

    #[serde(rename = "More", default)]
    pub more: bool,

    #[serde(rename = "AnchorID")]
    pub anchor_id: Option<LinkId>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumChildItemDto {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub capture_time: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumChildrenResponse {
    pub photos: Vec<AlbumChildItemDto>,

    #[serde(rename = "More", default)]
    pub more: bool,

    #[serde(rename = "AnchorID")]
    pub anchor_id: Option<LinkId>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumLinkCreationFields {
    pub name: PgpArmoredMessage,

    #[serde(rename = "Hash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub name_hash_digest: Vec<u8>,

    pub node_key: PgpArmoredPrivateKey,
    pub node_passphrase: PgpArmoredMessage,
    pub node_passphrase_signature: PgpArmoredSignature,
    pub signature_email: String,
    pub node_hash_key: PgpArmoredMessage,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub x_attr: Option<PgpArmoredMessage>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumCreationRequest {
    pub locked: bool,
    pub link: AlbumLinkCreationFields,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumCreationLinkId {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumCreationResponseAlbum {
    pub link: AlbumCreationLinkId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumCreationResponse {
    pub album: AlbumCreationResponseAlbum,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumNameUpdate {
    pub name: PgpArmoredMessage,

    #[serde(rename = "Hash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub name_hash_digest: Vec<u8>,

    #[serde(rename = "OriginalHash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub original_name_hash_digest: Vec<u8>,

    pub name_signature_email: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AlbumUpdateRequest {
    #[serde(rename = "CoverLinkID")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_link_id: Option<LinkId>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<AlbumNameUpdate>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AddPhotoToAlbumItem {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    #[serde(rename = "Hash")]
    pub name_hash: String,

    pub name: PgpArmoredMessage,
    pub name_signature_email: String,
    pub node_passphrase: PgpArmoredMessage,
    pub content_hash: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AddPhotosToAlbumRequest {
    pub album_data: Vec<AddPhotoToAlbumItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AddPhotoToAlbumResult {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    pub response: ApiResponse,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AddPhotosToAlbumResponse {
    #[serde(default)]
    pub responses: Vec<AddPhotoToAlbumResult>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct RemovePhotosFromAlbumRequest {
    #[serde(rename = "LinkIDs")]
    pub link_ids: Vec<LinkId>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PhotoTagsRequest {
    pub tags: Vec<PhotoTag>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct FavoritePhotoPayloadRelated {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,
    pub name: PgpArmoredMessage,
    pub name_signature_email: String,
    pub node_passphrase: PgpArmoredMessage,
    pub content_hash: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct FavoritePhotoData {
    pub name: PgpArmoredMessage,
    pub name_signature_email: String,
    pub node_passphrase: PgpArmoredMessage,
    pub content_hash: String,
    #[serde(rename = "Hash")]
    pub name_hash: String,
    pub related_photos: Vec<FavoritePhotoPayloadRelated>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct FavoritePhotoPayload {
    pub photo_data: FavoritePhotoData,
}

#[derive(Debug, Clone)]
pub struct AlbumInfo {
    pub uid: NodeUid,
    pub photo_count: u64,
    pub last_activity_time: DateTime<Utc>,
    pub cover_uid: Option<NodeUid>,
}

#[derive(Debug, Clone)]
pub struct AlbumChildItem {
    pub uid: NodeUid,
    pub capture_time: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct PhotoTagUpdate {
    pub uid: NodeUid,
    pub tags_to_add: Vec<PhotoTag>,
    pub tags_to_remove: Vec<PhotoTag>,
}

pub struct DefaultPhotosApiClient {
    client: ClientWithMiddleware,
    base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultPhotosApiClient {
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
            let (access_token, _): (String, String) = credential.get_tokens().await?;
            builder = builder.header("Authorization", format!("Bearer {}", access_token));
            builder = builder.header("x-pm-uid", credential.session_id().raw());
        }
        Ok(builder)
    }
}

#[async_trait]
impl PhotosApiClient for DefaultPhotosApiClient {
    async fn create_volume(
        &self,
        request: PhotosVolumeCreationRequest,
    ) -> anyhow::Result<VolumeCreationResponse> {
        let url = self.base_url.join("photos/volumes")?;
        let builder = self.client.post(url).json(&request);
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder
            .send()
            .await?
            .json::<VolumeCreationResponse>()
            .await?)
    }

    async fn get_root_share(&self) -> anyhow::Result<ShareResponseV2> {
        let url = self.base_url.join("v2/shares/photos")?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder.send().await?.json::<ShareResponseV2>().await?)
    }

    async fn get_timeline_photos(
        &self,
        request: TimelinePhotoListRequest,
    ) -> anyhow::Result<TimelinePhotoListResponse> {
        let path = match &request.previous_page_last_link_id {
            Some(anchor) => format!(
                "volumes/{}/photos?PreviousPageLastLinkID={}",
                request.volume_id.raw(),
                anchor.raw()
            ),
            None => format!("volumes/{}/photos", request.volume_id.raw()),
        };
        let url = self.base_url.join(&path)?;
        let builder = self.client.get(url);
        let builder = self.add_auth_headers(builder).await?;

        Ok(builder
            .send()
            .await?
            .json::<TimelinePhotoListResponse>()
            .await?)
    }

    async fn check_duplicates(
        &self,
        volume_id: VolumeId,
        name_hashes: Vec<String>,
    ) -> anyhow::Result<CheckDuplicatesResponse> {
        let url = self
            .base_url
            .join(&format!("volumes/{}/photos/duplicates", volume_id.raw()))?;
        let builder = self
            .client
            .post(url)
            .json(&CheckDuplicatesRequest { name_hashes });
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder.send().await?.json::<CheckDuplicatesResponse>().await?)
    }

    async fn get_albums(
        &self,
        volume_id: VolumeId,
        anchor_id: Option<LinkId>,
    ) -> anyhow::Result<AlbumsListResponse> {
        let path = match anchor_id {
            Some(anchor) => format!(
                "photos/volumes/{}/albums?AnchorID={}",
                volume_id.raw(),
                anchor.raw()
            ),
            None => format!("photos/volumes/{}/albums", volume_id.raw()),
        };
        let url = self.base_url.join(&path)?;
        let builder = self.add_auth_headers(self.client.get(url)).await?;
        Ok(builder.send().await?.json::<AlbumsListResponse>().await?)
    }

    async fn get_album_children(
        &self,
        volume_id: VolumeId,
        album_link_id: LinkId,
        anchor_id: Option<LinkId>,
    ) -> anyhow::Result<AlbumChildrenResponse> {
        let path = match anchor_id {
            Some(anchor) => format!(
                "photos/volumes/{}/albums/{}/children?Sort=Captured&Desc=1&AnchorID={}",
                volume_id.raw(),
                album_link_id.raw(),
                anchor.raw()
            ),
            None => format!(
                "photos/volumes/{}/albums/{}/children?Sort=Captured&Desc=1",
                volume_id.raw(),
                album_link_id.raw()
            ),
        };
        let url = self.base_url.join(&path)?;
        let builder = self.add_auth_headers(self.client.get(url)).await?;
        Ok(builder.send().await?.json::<AlbumChildrenResponse>().await?)
    }

    async fn create_album(
        &self,
        volume_id: VolumeId,
        request: AlbumCreationRequest,
    ) -> anyhow::Result<AlbumCreationResponse> {
        let url = self
            .base_url
            .join(&format!("photos/volumes/{}/albums", volume_id.raw()))?;
        let builder = self.add_auth_headers(self.client.post(url).json(&request)).await?;
        Ok(builder.send().await?.json::<AlbumCreationResponse>().await?)
    }

    async fn update_album(
        &self,
        volume_id: VolumeId,
        album_link_id: LinkId,
        request: AlbumUpdateRequest,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "photos/volumes/{}/albums/{}",
            volume_id.raw(),
            album_link_id.raw()
        ))?;
        let builder = self.add_auth_headers(self.client.put(url).json(&request)).await?;
        builder.send().await?;
        Ok(())
    }

    async fn delete_album(
        &self,
        volume_id: VolumeId,
        album_link_id: LinkId,
        force: bool,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "photos/volumes/{}/albums/{}?DeleteAlbumPhotos={}",
            volume_id.raw(),
            album_link_id.raw(),
            if force { 1 } else { 0 }
        ))?;
        let builder = self.add_auth_headers(self.client.delete(url)).await?;
        builder.send().await?;
        Ok(())
    }

    async fn add_photos_to_album(
        &self,
        volume_id: VolumeId,
        album_link_id: LinkId,
        request: AddPhotosToAlbumRequest,
    ) -> anyhow::Result<AddPhotosToAlbumResponse> {
        let url = self.base_url.join(&format!(
            "photos/volumes/{}/albums/{}/add-multiple",
            volume_id.raw(),
            album_link_id.raw()
        ))?;
        let builder = self.add_auth_headers(self.client.post(url).json(&request)).await?;
        Ok(builder.send().await?.json::<AddPhotosToAlbumResponse>().await?)
    }

    async fn remove_photos_from_album(
        &self,
        volume_id: VolumeId,
        album_link_id: LinkId,
        link_ids: Vec<LinkId>,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "photos/volumes/{}/albums/{}/remove-multiple",
            volume_id.raw(),
            album_link_id.raw()
        ))?;
        let request = RemovePhotosFromAlbumRequest { link_ids };
        let builder = self.add_auth_headers(self.client.post(url).json(&request)).await?;
        builder.send().await?;
        Ok(())
    }

    async fn add_photo_tags(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        tags: Vec<PhotoTag>,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "photos/volumes/{}/links/{}/tags",
            volume_id.raw(),
            link_id.raw()
        ))?;
        let builder = self
            .add_auth_headers(self.client.post(url).json(&PhotoTagsRequest { tags }))
            .await?;
        builder.send().await?;
        Ok(())
    }

    async fn remove_photo_tags(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        tags: Vec<PhotoTag>,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "photos/volumes/{}/links/{}/tags",
            volume_id.raw(),
            link_id.raw()
        ))?;
        let builder = self
            .add_auth_headers(self.client.delete(url).json(&PhotoTagsRequest { tags }))
            .await?;
        builder.send().await?;
        Ok(())
    }

    async fn set_photo_favorite(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        payload: Option<FavoritePhotoPayload>,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "photos/volumes/{}/links/{}/favorite",
            volume_id.raw(),
            link_id.raw()
        ))?;
        let builder = match payload {
            Some(p) => self.client.post(url).json(&p),
            None => self.client.post(url),
        };
        let builder = self.add_auth_headers(builder).await?;
        builder.send().await?;
        Ok(())
    }
}

pub struct PhotosLinksApiClient {
    client: ClientWithMiddleware,
    base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
    drive: DefaultLinksApiClient,
}

impl PhotosLinksApiClient {
    pub fn new(
        client: ClientWithMiddleware,
        base_url: reqwest::Url,
        token_credential: Option<TokenCredential>,
    ) -> Self {
        Self {
            drive: DefaultLinksApiClient::new(
                client.clone(),
                base_url.clone(),
                token_credential.clone(),
            ),
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
            let (access_token, _): (String, String) = credential.get_tokens().await?;
            builder = builder.header("Authorization", format!("Bearer {}", access_token));
            builder = builder.header("x-pm-uid", credential.session_id().raw());
        }
        Ok(builder)
    }
}

#[async_trait]
impl LinksApiClient for PhotosLinksApiClient {
    /// Overridden — uses photos-specific endpoint.
    async fn get_details(
        &self,
        volume_id: VolumeId,
        link_ids: Vec<LinkId>,
    ) -> anyhow::Result<LinkDetailsResponse> {
        let url = self
            .base_url
            .join(&format!("photos/volumes/{}/links", volume_id.raw()))?;
        let builder = self
            .client
            .post(url)
            .json(&LinkDetailsRequest::new(link_ids));
        let builder = self.add_auth_headers(builder).await?;
        Ok(builder.send().await?.json::<LinkDetailsResponse>().await?)
    }

    /// Delegated to drive implementation.
    async fn get_context_share(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
    ) -> anyhow::Result<ContextShareResponse> {
        self.drive.get_context_share(volume_id, link_id).await
    }

    async fn move_link(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: MoveSingleLinkRequest,
    ) -> anyhow::Result<ApiResponse> {
        self.drive.move_link(volume_id, link_id, request).await
    }

    async fn move_multiple(
        &self,
        volume_id: VolumeId,
        request: MoveMultipleLinksRequest,
    ) -> anyhow::Result<ApiResponse> {
        self.drive.move_multiple(volume_id, request).await
    }

    async fn rename(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: RenameLinkRequest,
    ) -> anyhow::Result<ApiResponse> {
        self.drive.rename(volume_id, link_id, request).await
    }

    async fn delete_multiple(
        &self,
        volume_id: VolumeId,
        link_ids: Vec<LinkId>,
    ) -> anyhow::Result<AggregateApiResponse<LinkIdResponsePair>> {
        self.drive.delete_multiple(volume_id, link_ids).await
    }

    async fn get_available_names(
        &self,
        volume_id: VolumeId,
        folder_id: LinkId,
        request: NodeNameAvailabilityRequest,
    ) -> anyhow::Result<NodeNameAvailabilityResponse> {
        self.drive
            .get_available_names(volume_id, folder_id, request)
            .await
    }

    async fn copy_link(
        &self,
        volume_id: VolumeId,
        link_id: LinkId,
        request: CopyLinkRequest,
    ) -> anyhow::Result<CopyLinkResponse> {
        self.drive.copy_link(volume_id, link_id, request).await
    }
}

/// Concrete implementation of `DriveApiClients` for the Photos context.
pub struct PhotosApiClients {
    volumes: Arc<DefaultVolumesApiClient>,
    shares: Arc<DefaultSharesApiClient>,
    links: Arc<dyn LinksApiClient + Send + Sync>,
    folders: Arc<DefaultFoldersApiClient>,
    files: Arc<DefaultFilesApiClient>,
    storage: Arc<DefaultStorageApiClient>,
    trash: Arc<DefaultTrashApiClient>,
    events: Arc<DefaultEventsApiClient>,
    devices: Arc<crate::api::devices::DefaultDevicesApiClient>,
}

impl PhotosApiClients {
    pub fn new(
        default_client: ClientWithMiddleware,
        storage_client: ClientWithMiddleware,
        default_api_base_url: reqwest::Url,
        storage_api_base_url: reqwest::Url,
        token_credential: Option<TokenCredential>,
    ) -> Self {
        Self {
            volumes: Arc::new(DefaultVolumesApiClient::new(
                default_client.clone(),
                default_api_base_url.clone(),
                token_credential.clone(),
            )),
            shares: Arc::new(DefaultSharesApiClient::new(
                default_client.clone(),
                default_api_base_url.clone(),
                token_credential.clone(),
            )),
            links: Arc::new(PhotosLinksApiClient::new(
                default_client.clone(),
                default_api_base_url.clone(),
                token_credential.clone(),
            )),
            folders: Arc::new(DefaultFoldersApiClient::new(
                default_client.clone(),
                default_api_base_url.clone(),
                token_credential.clone(),
            )),
            files: Arc::new(DefaultFilesApiClient::new(
                default_client.clone(),
                default_api_base_url.clone(),
                token_credential.clone(),
            )),
            storage: Arc::new(DefaultStorageApiClient::new(
                default_client.clone(),
                storage_client,
                default_api_base_url.clone(),
                storage_api_base_url,
                token_credential.clone(),
            )),
            trash: Arc::new(DefaultTrashApiClient::new(
                default_client.clone(),
                default_api_base_url.clone(),
                token_credential.clone(),
            )),
            events: Arc::new(DefaultEventsApiClient::new(
                default_client.clone(),
                default_api_base_url.clone(),
                token_credential.clone(),
            )),
            devices: Arc::new(crate::api::devices::DefaultDevicesApiClient::new(
                default_client,
                default_api_base_url,
                token_credential,
            )),
        }
    }
}

impl DriveApiClients for PhotosApiClients {
    fn volumes(&self) -> Arc<dyn VolumesApiClient> {
        self.volumes.clone()
    }
    fn shares(&self) -> Arc<dyn SharesApiClient> {
        self.shares.clone()
    }
    fn links(&self) -> Arc<dyn LinksApiClient> {
        self.links.clone()
    }
    fn folders(&self) -> Arc<dyn FoldersApiClient> {
        self.folders.clone()
    }
    fn files(&self) -> Arc<dyn FilesApiClient> {
        self.files.clone()
    }
    fn storage(&self) -> Arc<dyn StorageApiClient> {
        self.storage.clone()
    }
    fn trash(&self) -> Arc<dyn TrashApiClient> {
        self.trash.clone()
    }
    fn events(&self) -> Arc<dyn EventsApiClient> {
        self.events.clone()
    }

    fn devices(&self) -> Arc<dyn DevicesApiClient> {
        self.devices.clone()
    }
}
