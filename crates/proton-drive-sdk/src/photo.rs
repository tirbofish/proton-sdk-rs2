use crate::api::events::{CoreEventsResponse, VolumeEventsResponse};
use crate::api::file::photos::{
    AddPhotoToAlbumItem, AddPhotosToAlbumRequest, AlbumChildItem, AlbumCreationRequest, AlbumInfo,
    AlbumLinkCreationFields, AlbumNameUpdate, AlbumUpdateRequest, DefaultPhotosApiClient,
    FavoritePhotoData, FavoritePhotoPayload, PhotoTagUpdate, PhotosApiClient, PhotosApiClients,
    TimelinePhotoListRequest,
};
use crate::api::{DriveApiClients, DriveApiClientsFactory};
use crate::cache::entity::{DefaultPhotosEntityCache, PhotosEntityCache};
use crate::client::{ProtonDriveClient, ProtonDriveDefaults};
use crate::links::LinkId;
use crate::node::DtoToMetadataConverter;
use crate::node::crypto::NodeCrypto;
use crate::node::file::download::FileDownloader;
use crate::node::file::upload::FileUploader;
use crate::node::folder::{FolderNode, FolderOperations, FolderSecrets};
use crate::node::photo::{PhotoTag, PhotosFileUploadMetadata, PhotosTimelineItem, TimelineEntry};
use crate::node::{DegradedNode, Node, NodeAndSecrets, NodeUid};
use crate::pgp::PgpPrivateKey;
use crate::share_ops::{ShareOperations, SharingOperations};
use crate::utils::PotentialObject;
use crate::volume::VolumeId;
use hmac::{Hmac, KeyInit, Mac};
use proton_sdk_rs2::auth::TokenCredential;
use proton_sdk_rs2::session::ProtonAPISession;
use std::sync::Arc;

type HmacSha256 = Hmac<sha2::Sha256>;

struct PhotosApiClientsFactory;

impl DriveApiClientsFactory for PhotosApiClientsFactory {
    fn create(
        &self,
        default_api_http_client: reqwest_middleware::ClientWithMiddleware,
        storage_api_http_client: reqwest_middleware::ClientWithMiddleware,
        default_api_base_url: reqwest::Url,
        storage_api_base_url: reqwest::Url,
        token_credential: Option<TokenCredential>,
    ) -> Arc<dyn DriveApiClients> {
        Arc::new(PhotosApiClients::new(
            default_api_http_client,
            storage_api_http_client,
            default_api_base_url,
            storage_api_base_url,
            token_credential,
        ))
    }
}

pub struct ProtonPhotosClient {
    /// Inner drive client configured with the Photos API clients.
    drive: ProtonDriveClient,
    /// Photos-specific API surface (timeline, share bootstrap).
    photos_api: DefaultPhotosApiClient,
    /// Photos-specific entity cache (volume/share IDs are stored separately
    /// from the main-drive entity cache).
    photos_entities: Arc<DefaultPhotosEntityCache>,
}

impl ProtonPhotosClient {
    /// Construct from an authenticated [`ProtonAPISession`].
    pub fn new(session: &ProtonAPISession, uid: Option<String>) -> anyhow::Result<Self> {
        let drive = ProtonDriveClient::from_session_with_drive_api_clients_factory(
            session,
            Arc::new(PhotosApiClientsFactory),
            uid,
        )?;

        // Build the photos-specific API client and entity cache using the same
        // HTTP credentials and repository as the main drive client.
        use proton_sdk_rs2::client::ProtonApiDefaults;
        let http_client = session.get_http_client(
            Some(ProtonDriveDefaults::DRIVE_BASE_ROUTE.to_string()),
            Some(std::time::Duration::from_secs(
                ProtonApiDefaults::DEFAULT_TIMEOUT_SECONDS as u64,
            )),
            None,
        )?;
        let base_url = reqwest::Url::parse(&session.client_config.base_url.to_string())?
            .join(ProtonDriveDefaults::DRIVE_BASE_ROUTE)?;
        let photos_api = DefaultPhotosApiClient::new(
            reqwest_middleware::ClientBuilder::new(http_client).build(),
            base_url,
            Some(session.token_credential.clone()),
        );
        let photos_entities = Arc::new(DefaultPhotosEntityCache::new(
            session.client_config.entity_cache_repository.clone(),
        ));

        Ok(Self {
            drive,
            photos_api,
            photos_entities,
        })
    }

    /// Resolves the Photos root folder, caching the share and volume IDs for
    /// subsequent calls.
    pub async fn get_photos_root_folder(&self) -> anyhow::Result<FolderNode> {
        // Fast path: share already cached.
        if let Some(share_id) = self.photos_entities.try_get_photos_share_id().await? {
            let share_and_key = ShareOperations::get_share(&self.drive, share_id).await?;
            let root_folder_id = share_and_key.share.root_folder_id.clone();
            let metadata = DtoToMetadataConverter::get_fresh_node_metadata(
                &self.drive,
                root_folder_id,
                Some(share_and_key),
            )
            .await?;
            return metadata.get_folder_node_or_throw();
        }

        // Slow path: fetch + decrypt the Photos share.
        let share_response = self.photos_api.get_root_share().await?;
        let (volume_dto, share_dto, link_details) = share_response.deconstruct();

        let (share, share_key) = crate::share_ops::ShareCrypto::decrypt_share(
            &self.drive,
            share_dto.id.clone(),
            &share_dto.key,
            &share_dto.passphrase,
            &share_dto.passphrase_signature,
            share_dto
                .invitee_share_passphrase_session_key_signature
                .as_ref(),
            &share_dto.creator_email_address,
            &share_dto.address_id,
        )
        .await?;

        self.photos_entities
            .set_photos_volume_id(volume_dto.id.clone())
            .await?;
        self.photos_entities
            .set_photos_share_id(share_dto.id.clone())
            .await?;
        self.drive
            .cache()
            .entities()
            .set_share(share.clone())
            .await?;
        self.drive
            .cache()
            .secrets()
            .set_share_key(share_dto.id.clone(), share_key.clone())
            .await?;

        let metadata_result = DtoToMetadataConverter::convert_dto_to_node_metadata(
            self.drive.account().clone(),
            self.drive.cache().entities().as_ref(),
            self.drive.cache().secrets().as_ref(),
            volume_dto.id.clone(),
            link_details,
            Some(&share_key),
        )
        .await?;

        if let PotentialObject::Node(ref metadata) = metadata_result {
            if let NodeAndSecrets::Folder(_, ref secrets) = metadata.inner {
                self.drive
                    .cache()
                    .secrets()
                    .set_folder_secrets(
                        metadata.node().uid().clone(),
                        PotentialObject::Node(secrets.clone()),
                    )
                    .await?;
            }
        }

        metadata_result.get_folder_node_or_throw()
    }

    /// Returns the cached Photos volume ID, resolving it first if needed.
    pub async fn get_photos_volume_id(&self) -> anyhow::Result<VolumeId> {
        if let Some(vid) = self.photos_entities.try_get_photos_volume_id().await? {
            return Ok(vid);
        }
        // Bootstrapping the root folder caches the volume ID as a side-effect.
        self.get_photos_root_folder().await?;
        self.photos_entities
            .try_get_photos_volume_id()
            .await?
            .ok_or_else(|| anyhow::anyhow!("Photos volume ID unavailable after bootstrap"))
    }

    /// Iterates through the entire photo timeline in chronological pages and
    /// returns a flat `Vec<PhotosTimelineItem>`. Each page is fetched eagerly;
    /// use [`poll_volume_events`] to receive incremental updates afterwards.
    pub async fn iterate_timeline(&self) -> anyhow::Result<Vec<PhotosTimelineItem>> {
        let volume_id: VolumeId = self.get_photos_volume_id().await?;
        let mut items: Vec<PhotosTimelineItem> = Vec::new();
        let mut cursor: Option<LinkId> = None;

        loop {
            let request = TimelinePhotoListRequest {
                volume_id: volume_id.clone(),
                previous_page_last_link_id: cursor.clone(),
            };
            let response = self.photos_api.get_timeline_photos(request).await?;
            let page_len = response.photos.len();

            for dto in &response.photos {
                cursor = Some(dto.id.clone());
                items.push(PhotosTimelineItem {
                    uid: NodeUid::new(volume_id.clone(), dto.id.clone()),
                    capture_time: dto.capture_time,
                });
            }

            // The API returns up to 500 items per page. An under-full page means
            // we have reached the end.
            if page_len < 500 {
                break;
            }
        }

        Ok(items)
    }

    /// Fetch a single node by UID (file or folder).
    pub async fn get_node(
        &self,
        uid: NodeUid,
    ) -> anyhow::Result<PotentialObject<Node, DegradedNode>> {
        self.drive.get_node(uid).await
    }

    /// Fetch multiple nodes, streaming results as they are decrypted.
    pub async fn enumerate_nodes(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        use futures::StreamExt;
        let mut stream = self.drive.enumerate_nodes(uids).await?;
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item?);
        }
        Ok(out)
    }

    /// List direct children of a folder by its link ID (pass `None` for the
    /// Photos root folder).
    pub async fn list_children(
        &self,
        volume_id: VolumeId,
        parent_link_id: Option<LinkId>,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        self.drive.list_children(volume_id, parent_link_id).await
    }

    /// Streaming version of [`list_children`]: returns a stream that yields
    /// items as they are fetched and decrypted, without waiting for all of them.
    pub async fn enumerate_children(
        &self,
        volume_id: VolumeId,
        parent_link_id: Option<LinkId>,
    ) -> anyhow::Result<
        impl futures::Stream<Item = anyhow::Result<PotentialObject<Node, DegradedNode>>> + 'static,
    > {
        let uid = match parent_link_id {
            Some(id) => NodeUid::new(volume_id, id),
            None => {
                let root = self.get_photos_root_folder().await?;
                root.base.uid
            }
        };
        self.drive.enumerate_folder_children(uid).await
    }

    /// Creates a file uploader for a new photo in the given parent folder.
    ///
    /// `media_type` should be a MIME type such as `"image/jpeg"`.  The upload
    /// is committed when [`FileUploader::finish`] is called on the result.
    pub async fn get_file_uploader(
        &self,
        parent_uid: NodeUid,
        name: String,
        media_type: String,
        size: i64,
        metadata: PhotosFileUploadMetadata,
    ) -> anyhow::Result<FileUploader> {
        self.drive
            .get_file_uploader(
                parent_uid,
                name,
                media_type,
                size,
                metadata
                    .base
                    .last_modification_time
                    .map(|dt| std::time::SystemTime::from(dt)),
                metadata.base.additional_metadata,
                None,  // media_info
                false, // override_existing_draft_by_other_client
            )
            .await
    }

    /// Creates a downloader for the active revision of a photo or file node.
    pub async fn get_file_downloader(&self, uid: NodeUid) -> anyhow::Result<FileDownloader> {
        let node = self.drive.get_node(uid.clone()).await?;
        let resolved = node
            .result()
            .map_err(|e| anyhow::anyhow!("Cannot download node {}: {}", uid.link_id.raw(), e))?;
        let revision_uid = match resolved {
            Node::File(f) | Node::Photo(f) => f.active_revision.uid.clone(),
            _ => anyhow::bail!("Node {:?} is not a file/photo", uid),
        };
        self.drive.get_file_downloader(revision_uid).await
    }

    /// Rename a node.
    pub async fn rename_node(
        &self,
        uid: NodeUid,
        new_name: String,
        new_media_type: Option<String>,
    ) -> anyhow::Result<()> {
        self.drive.rename_node(uid, new_name, new_media_type).await
    }

    /// Move nodes to a new parent folder.
    pub async fn move_nodes(
        &self,
        uids: Vec<NodeUid>,
        new_parent_uid: NodeUid,
    ) -> anyhow::Result<()> {
        self.drive.move_nodes(uids, new_parent_uid).await
    }

    /// Server-side copy of a node into a different (or the same) folder,
    /// optionally renaming it.
    pub async fn copy_node(
        &self,
        uid: NodeUid,
        new_parent_uid: NodeUid,
        new_name: Option<String>,
    ) -> anyhow::Result<LinkId> {
        self.drive.copy_node(uid, new_parent_uid, new_name).await
    }

    /// Create a new folder under `parent_uid`.
    pub async fn create_folder(
        &self,
        parent_uid: NodeUid,
        name: String,
    ) -> anyhow::Result<FolderNode> {
        self.drive.create_folder(parent_uid, name, None).await
    }

    /// Move nodes to the trash (soft delete).
    pub async fn trash_nodes(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<std::collections::HashMap<NodeUid, Result<(), anyhow::Error>>> {
        self.drive.trash_nodes(uids).await
    }

    /// Restore previously trashed nodes.
    pub async fn restore_nodes(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<std::collections::HashMap<NodeUid, Result<(), anyhow::Error>>> {
        self.drive.restore_nodes(uids).await
    }

    /// Permanently delete nodes from the trash.
    pub async fn delete_nodes(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<std::collections::HashMap<NodeUid, Result<(), anyhow::Error>>> {
        self.drive.delete_nodes(uids).await
    }

    /// Permanently delete all nodes in the Photos trash.
    pub async fn empty_trash(&self) -> anyhow::Result<()> {
        self.drive.empty_trash().await
    }

    /// List all trashed photo nodes.
    pub async fn enumerate_trash(&self) -> anyhow::Result<Vec<Result<Node, DegradedNode>>> {
        self.drive.enumerate_trash().await
    }

    /// Returns the latest event-ID for the Photos volume. Use this as the
    /// starting cursor for [`poll_volume_events`].
    pub async fn get_volume_latest_event_id(&self, volume_id: VolumeId) -> anyhow::Result<String> {
        self.drive
            .api()
            .events()
            .get_volume_latest_event_id(volume_id)
            .await
    }

    /// Poll for changes since `event_id`. Returns the next cursor and any node
    /// change events to be applied to the local cache.
    pub async fn poll_volume_events(
        &self,
        volume_id: VolumeId,
        event_id: &str,
    ) -> anyhow::Result<VolumeEventsResponse> {
        self.drive
            .api()
            .events()
            .get_volume_events(volume_id, event_id)
            .await
    }

    /// Returns the latest global core event-ID.
    pub async fn get_core_latest_event_id(&self) -> anyhow::Result<String> {
        self.drive.api().events().get_core_latest_event_id().await
    }

    /// Poll for core-level events since `event_id`.
    pub async fn poll_core_events(&self, event_id: &str) -> anyhow::Result<CoreEventsResponse> {
        self.drive.api().events().get_core_events(event_id).await
    }

    /// Fetch one page of the photos timeline, returning raw entries with tags
    /// and an optional cursor for the next page.
    ///
    /// Pass `cursor = None` for the first page, then the returned `next_cursor`
    /// for each subsequent page. `next_cursor = None` means this was the last page.
    pub async fn get_timeline_page(
        &self,
        volume_id: &VolumeId,
        cursor: Option<&LinkId>,
    ) -> anyhow::Result<(Vec<TimelineEntry>, Option<LinkId>)> {
        use crate::api::file::photos::TimelinePhotoListRequest;
        let request = TimelinePhotoListRequest {
            volume_id: volume_id.clone(),
            previous_page_last_link_id: cursor.cloned(),
        };
        let response = self.photos_api.get_timeline_photos(request).await?;
        let is_last = response.photos.len() < 500;
        let next_cursor = if is_last {
            None
        } else {
            response.photos.last().map(|p| p.id.clone())
        };
        let entries = response
            .photos
            .into_iter()
            .map(|dto| TimelineEntry {
                uid: NodeUid::new(volume_id.clone(), dto.id),
                capture_time: dto.capture_time,
                tags: dto.tags,
            })
            .collect();
        Ok((entries, next_cursor))
    }

    /// Access the underlying [`ProtonDriveClient`] for operations not yet
    /// surfaced by [`ProtonPhotosClient`] (e.g. block-level upload/download).
    pub fn drive(&self) -> &ProtonDriveClient {
        &self.drive
    }

    /// Checks for existing photos that match both the name and SHA-1 content hash.
    /// Returns the UIDs of any matching duplicates, or an empty Vec when none are found.
    pub async fn find_photo_duplicates(
        &self,
        name: &str,
        sha1_hex: &str,
    ) -> anyhow::Result<Vec<NodeUid>> {
        let volume_id = self.get_photos_volume_id().await?;
        let root = self.get_photos_root_folder().await?;

        let hash_key = self.get_root_hash_key(&root.base.uid).await?;

        let name_hash = {
            let mut mac = HmacSha256::new_from_slice(&hash_key)
                .map_err(|_| anyhow::anyhow!("Invalid hash key length"))?;
            mac.update(name.as_bytes());
            hex::encode(mac.finalize().into_bytes())
        };

        let response = self
            .photos_api
            .check_duplicates(volume_id.clone(), vec![name_hash.clone()])
            .await?;

        let candidates: Vec<_> = response
            .duplicate_hashes
            .into_iter()
            .filter(|d| d.link_state == 1 && d.name_hash == name_hash && d.content_hash.is_some())
            .collect();

        if candidates.is_empty() {
            return Ok(vec![]);
        }

        let content_hash = {
            let mut mac = HmacSha256::new_from_slice(&hash_key)
                .map_err(|_| anyhow::anyhow!("Invalid hash key length"))?;
            mac.update(sha1_hex.as_bytes());
            hex::encode(mac.finalize().into_bytes())
        };

        Ok(candidates
            .into_iter()
            .filter(|d| d.content_hash.as_deref() == Some(&content_hash))
            .map(|d| NodeUid::new(volume_id.clone(), d.link_id))
            .collect())
    }

    /// Returns true when a photo with the same name and SHA-1 content already exists in the timeline.
    pub async fn is_duplicate_photo(&self, name: &str, sha1_hex: &str) -> anyhow::Result<bool> {
        Ok(!self.find_photo_duplicates(name, sha1_hex).await?.is_empty())
    }

    /// Returns a list of all albums in the photos volume, paginated internally.
    pub async fn iterate_albums(&self) -> anyhow::Result<Vec<AlbumInfo>> {
        let volume_id = self.get_photos_volume_id().await?;
        let mut albums = Vec::new();
        let mut anchor: Option<LinkId> = None;

        loop {
            let response = self
                .photos_api
                .get_albums(volume_id.clone(), anchor.clone())
                .await?;

            for dto in &response.albums {
                albums.push(AlbumInfo {
                    uid: NodeUid::new(volume_id.clone(), dto.link_id.clone()),
                    photo_count: dto.photo_count,
                    last_activity_time: dto.last_activity_time,
                    cover_uid: dto
                        .cover_link_id
                        .clone()
                        .map(|id| NodeUid::new(volume_id.clone(), id)),
                });
            }

            if !response.more || response.anchor_id.is_none() {
                break;
            }
            anchor = response.anchor_id;
        }

        Ok(albums)
    }

    /// Returns all photo entries inside an album, sorted by capture time descending.
    pub async fn iterate_album(&self, album_uid: NodeUid) -> anyhow::Result<Vec<AlbumChildItem>> {
        let volume_id = album_uid.volume_id.clone();
        let mut items = Vec::new();
        let mut anchor: Option<LinkId> = None;

        loop {
            let response = self
                .photos_api
                .get_album_children(volume_id.clone(), album_uid.link_id.clone(), anchor.clone())
                .await?;

            for dto in &response.photos {
                items.push(AlbumChildItem {
                    uid: NodeUid::new(volume_id.clone(), dto.link_id.clone()),
                    capture_time: dto.capture_time,
                });
            }

            if !response.more || response.anchor_id.is_none() {
                break;
            }
            anchor = response.anchor_id;
        }

        Ok(items)
    }

    /// Creates a new album under the photos root folder and returns its `NodeUid`.
    pub async fn create_album(&self, name: String) -> anyhow::Result<NodeUid> {
        let volume_id = self.get_photos_volume_id().await?;
        let root = self.get_photos_root_folder().await?;
        let root_secrets = self.get_root_folder_secrets(&root.base.uid).await?;

        let signing_key = self.get_signing_key().await?;

        let album_key = crate::crypto::CryptoGenerator::generate_private_key()?;
        let album_passphrase = crate::crypto::CryptoGenerator::generate_passphrase();
        let _locked_album_key =
            album_key.to_armored_private_key(Some(album_passphrase.as_bytes()))?;

        let album_hash_key = crate::crypto::CryptoGenerator::generate_folder_hash_key().to_vec();

        let (encrypted_passphrase, passphrase_signature, _) =
            NodeCrypto::encrypt_and_sign_passphrase(
                album_passphrase.as_bytes(),
                &root_secrets.base.key,
                &signing_key,
            )?;

        let (encrypted_name, name_hash_digest, _) = NodeCrypto::encrypt_and_sign_name(
            &name,
            &root_secrets.hash_key,
            &root_secrets.base.key,
            &signing_key,
        )?;

        let encrypted_album_hash_key = NodeCrypto::encrypt_folder_hash_key(
            &crate::pgp::PgpPrivateKey(album_key.0.clone()),
            &album_hash_key,
            &signing_key,
        )?;

        let default_address = self.drive.account().get_default_address().await?;

        let locked_album_key =
            album_key.to_armored_private_key(Some(album_passphrase.as_bytes()))?;

        let request = AlbumCreationRequest {
            locked: false,
            link: AlbumLinkCreationFields {
                name: encrypted_name,
                name_hash_digest,
                node_key: locked_album_key,
                node_passphrase: encrypted_passphrase,
                node_passphrase_signature: passphrase_signature
                    .ok_or_else(|| anyhow::anyhow!("Passphrase signature missing"))?,
                signature_email: default_address.email_address.clone(),
                node_hash_key: encrypted_album_hash_key,
                x_attr: None,
            },
        };

        let response = self
            .photos_api
            .create_album(volume_id.clone(), request)
            .await?;

        Ok(NodeUid::new(volume_id, response.album.link.link_id))
    }

    /// Updates an existing album: optionally renames it and/or sets a new cover photo.
    pub async fn update_album(
        &self,
        album_uid: NodeUid,
        name: Option<String>,
        cover_photo_uid: Option<NodeUid>,
    ) -> anyhow::Result<()> {
        let volume_id = album_uid.volume_id.clone();

        let name_update = if let Some(new_name) = name {
            let root = self.get_photos_root_folder().await?;
            let root_secrets = self.get_root_folder_secrets(&root.base.uid).await?;
            let signing_key = self.get_signing_key().await?;

            let metadata = DtoToMetadataConverter::get_fresh_node_metadata(
                &self.drive,
                album_uid.clone(),
                None,
            )
            .await?;
            let (_, _, _, original_name_hash_digest) = metadata.result()?.deconstruct();

            let album_secrets = self.get_root_folder_secrets(&album_uid).await?;

            let (encrypted_name, name_hash_digest, _) = NodeCrypto::encrypt_and_sign_name(
                &new_name,
                &root_secrets.hash_key,
                &root_secrets.base.key,
                &signing_key,
            )?;

            let default_address = self.drive.account().get_default_address().await?;
            let _ = album_secrets;

            Some(AlbumNameUpdate {
                name: encrypted_name,
                name_hash_digest,
                original_name_hash_digest,
                name_signature_email: default_address.email_address.clone(),
            })
        } else {
            None
        };

        let cover_link_id = cover_photo_uid.map(|uid| uid.link_id);

        self.photos_api
            .update_album(
                volume_id,
                album_uid.link_id,
                AlbumUpdateRequest {
                    cover_link_id,
                    link: name_update,
                },
            )
            .await
    }

    /// Permanently deletes an album. Photos in the timeline are kept; set `force` to also
    /// delete photos that exist only in the album and not in the timeline.
    pub async fn delete_album(&self, album_uid: NodeUid, force: bool) -> anyhow::Result<()> {
        let volume_id = album_uid.volume_id.clone();
        self.photos_api
            .delete_album(volume_id, album_uid.link_id, force)
            .await
    }

    /// Adds photos to an album by re-encrypting each photo's key material for the album context.
    pub async fn add_photos_to_album(
        &self,
        album_uid: NodeUid,
        photo_uids: Vec<NodeUid>,
    ) -> anyhow::Result<Vec<(NodeUid, bool)>> {
        if photo_uids.is_empty() {
            return Ok(vec![]);
        }

        let volume_id = album_uid.volume_id.clone();
        let album_secrets = self.get_root_folder_secrets(&album_uid).await?;
        let signing_key = self.get_signing_key().await?;
        let default_address = self.drive.account().get_default_address().await?;

        let link_ids: Vec<LinkId> = photo_uids.iter().map(|u| u.link_id.clone()).collect();
        let link_details_response = self
            .drive
            .api()
            .links()
            .get_details(volume_id.clone(), link_ids)
            .await?;

        let mut album_data = Vec::new();

        for link_details in &link_details_response.links {
            let photo_dto = link_details.photo.as_ref();
            let content_hash_bytes = photo_dto.and_then(|p| p.content_hash.as_ref());

            let node_uid = NodeUid::new(volume_id.clone(), link_details.link.id.clone());

            let meta = DtoToMetadataConverter::convert_dto_to_node_metadata(
                self.drive.account().clone(),
                self.drive.cache().entities().as_ref(),
                self.drive.cache().secrets().as_ref(),
                volume_id.clone(),
                link_details.clone(),
                None,
            )
            .await?;

            let (node, node_and_secrets, _, _) = match meta.result() {
                Ok(m) => m.deconstruct(),
                Err(_) => continue,
            };

            let (file_secrets, photo_name) = match (node, node_and_secrets) {
                (Node::File(ref f), NodeAndSecrets::File(_, s))
                | (Node::Photo(ref f), NodeAndSecrets::File(_, s)) => (s, f.base.base.name.clone()),
                _ => continue,
            };

            let content_hash = match content_hash_bytes {
                Some(bytes) => {
                    let sha1_hex = hex::encode(bytes);
                    let mut mac = HmacSha256::new_from_slice(&album_secrets.hash_key)
                        .map_err(|_| anyhow::anyhow!("Invalid album hash key"))?;
                    mac.update(sha1_hex.as_bytes());
                    hex::encode(mac.finalize().into_bytes())
                }
                None => {
                    anyhow::bail!("Photo {} has no content hash", node_uid);
                }
            };

            let mut name_mac = HmacSha256::new_from_slice(&album_secrets.hash_key)
                .map_err(|_| anyhow::anyhow!("Invalid album hash key"))?;
            name_mac.update(photo_name.as_bytes());
            let name_hash = hex::encode(name_mac.finalize().into_bytes());

            let encrypted_name = NodeCrypto::encrypt_name(
                &photo_name,
                &file_secrets.base.name_session_key,
                &album_secrets.base.key,
                &signing_key,
            )?;

            let encrypted_passphrase = NodeCrypto::reencrypt_passphrase(
                &file_secrets.base.passphrase_session_key.key,
                file_secrets.base.passphrase_pgp_session_key.as_ref(),
                &album_secrets.base.key,
                &signing_key,
            )?;

            album_data.push(AddPhotoToAlbumItem {
                link_id: node_uid.link_id.clone(),
                name_hash,
                name: encrypted_name,
                name_signature_email: default_address.email_address.clone(),
                node_passphrase: encrypted_passphrase,
                content_hash,
            });
        }

        let response = self
            .photos_api
            .add_photos_to_album(
                volume_id.clone(),
                album_uid.link_id,
                AddPhotosToAlbumRequest { album_data },
            )
            .await?;

        let results = response
            .responses
            .into_iter()
            .map(|r| {
                let uid = NodeUid::new(volume_id.clone(), r.link_id);
                let ok = r.response.is_success();
                (uid, ok)
            })
            .collect();

        Ok(results)
    }

    /// Removes photos from an album without deleting them from the timeline.
    pub async fn remove_photos_from_album(
        &self,
        album_uid: NodeUid,
        photo_uids: Vec<NodeUid>,
    ) -> anyhow::Result<()> {
        if photo_uids.is_empty() {
            return Ok(());
        }
        let volume_id = album_uid.volume_id.clone();
        let link_ids: Vec<LinkId> = photo_uids.into_iter().map(|u| u.link_id).collect();
        self.photos_api
            .remove_photos_from_album(volume_id, album_uid.link_id, link_ids)
            .await
    }

    /// Applies tag additions and removals to a batch of photos. For the Favorite tag,
    /// the photo's key material is re-encrypted for the root timeline volume.
    pub async fn update_photos(&self, updates: Vec<PhotoTagUpdate>) -> anyhow::Result<()> {
        let volume_id = self.get_photos_volume_id().await?;

        for update in updates {
            let link_id = update.uid.link_id.clone();

            let add_non_fav: Vec<PhotoTag> = update
                .tags_to_add
                .iter()
                .copied()
                .filter(|&t| t != PhotoTag::Favorite)
                .collect();

            let wants_favorite = update.tags_to_add.contains(&PhotoTag::Favorite);

            if !add_non_fav.is_empty() {
                self.photos_api
                    .add_photo_tags(volume_id.clone(), link_id.clone(), add_non_fav)
                    .await?;
            }

            if !update.tags_to_remove.is_empty() {
                self.photos_api
                    .remove_photo_tags(
                        volume_id.clone(),
                        link_id.clone(),
                        update.tags_to_remove.clone(),
                    )
                    .await?;
            }

            if wants_favorite {
                let payload = self
                    .build_favorite_payload(&update.uid, &volume_id)
                    .await
                    .ok()
                    .flatten();
                self.photos_api
                    .set_photo_favorite(volume_id.clone(), link_id, payload)
                    .await?;
            }
        }

        Ok(())
    }

    async fn get_root_hash_key(&self, root_uid: &NodeUid) -> anyhow::Result<Vec<u8>> {
        let secrets = self.get_root_folder_secrets(root_uid).await?;
        Ok(secrets.hash_key)
    }

    async fn get_root_folder_secrets(&self, uid: &NodeUid) -> anyhow::Result<FolderSecrets> {
        FolderOperations::get_secrets(&self.drive, uid.clone()).await
    }

    async fn get_signing_key(&self) -> anyhow::Result<PgpPrivateKey> {
        let default_address = self.drive.account().get_default_address().await?;
        let key = self
            .drive
            .account()
            .get_address_primary_private_key(&crate::account::AddressId::new(
                default_address.address_id.clone(),
            ))
            .await?;
        Ok(PgpPrivateKey(key))
    }

    async fn build_favorite_payload(
        &self,
        photo_uid: &NodeUid,
        volume_id: &VolumeId,
    ) -> anyhow::Result<Option<FavoritePhotoPayload>> {
        let root = self.get_photos_root_folder().await?;
        if photo_uid.volume_id == root.base.uid.volume_id {
            let parent_uid = {
                let node = self.drive.get_node(photo_uid.clone()).await?;
                node.result()
                    .map_err(|e| anyhow::anyhow!("{}", e))?
                    .base()
                    .parent_uid
                    .clone()
            };
            if parent_uid.as_ref() == Some(&root.base.uid) {
                return Ok(None);
            }
        }

        let root_secrets = self.get_root_folder_secrets(&root.base.uid).await?;
        let signing_key = self.get_signing_key().await?;
        let default_address = self.drive.account().get_default_address().await?;

        let link_details_response = self
            .drive
            .api()
            .links()
            .get_details(volume_id.clone(), vec![photo_uid.link_id.clone()])
            .await?;

        let link_details = link_details_response
            .links
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Photo not found"))?;

        let content_hash_bytes = link_details
            .photo
            .clone()
            .and_then(|p| p.content_hash)
            .ok_or_else(|| anyhow::anyhow!("Photo has no content hash"))?;

        let meta = DtoToMetadataConverter::convert_dto_to_node_metadata(
            self.drive.account().clone(),
            self.drive.cache().entities().as_ref(),
            self.drive.cache().secrets().as_ref(),
            volume_id.clone(),
            link_details,
            None,
        )
        .await?;

        let (node, node_and_secrets, _, _) = meta.result()?.deconstruct();

        let (file_secrets, photo_name) = match (node, node_and_secrets) {
            (Node::File(ref f), NodeAndSecrets::File(_, s))
            | (Node::Photo(ref f), NodeAndSecrets::File(_, s)) => (s, f.base.base.name.clone()),
            _ => anyhow::bail!("Node is not a file or photo"),
        };

        let sha1_hex = hex::encode(&content_hash_bytes);
        let mut content_mac = HmacSha256::new_from_slice(&root_secrets.hash_key)
            .map_err(|_| anyhow::anyhow!("Invalid hash key"))?;
        content_mac.update(sha1_hex.as_bytes());
        let content_hash = hex::encode(content_mac.finalize().into_bytes());

        let mut name_mac = HmacSha256::new_from_slice(&root_secrets.hash_key)
            .map_err(|_| anyhow::anyhow!("Invalid hash key"))?;
        name_mac.update(photo_name.as_bytes());
        let name_hash = hex::encode(name_mac.finalize().into_bytes());

        let encrypted_name = NodeCrypto::encrypt_name(
            &photo_name,
            &file_secrets.base.name_session_key,
            &root_secrets.base.key,
            &signing_key,
        )?;

        let encrypted_passphrase = NodeCrypto::reencrypt_passphrase(
            &file_secrets.base.passphrase_session_key.key,
            file_secrets.base.passphrase_pgp_session_key.as_ref(),
            &root_secrets.base.key,
            &signing_key,
        )?;

        Ok(Some(FavoritePhotoPayload {
            photo_data: FavoritePhotoData {
                name: encrypted_name,
                name_signature_email: default_address.email_address.clone(),
                node_passphrase: encrypted_passphrase,
                content_hash,
                name_hash,
                related_photos: vec![],
            },
        }))
    }

    /// UIDs of nodes the user has shared from the photos volume.
    pub async fn enumerate_shared_node_uids(&self) -> anyhow::Result<Vec<NodeUid>> {
        let volume_id = self.get_photos_volume_id().await?;
        SharingOperations::enumerate_shared_node_uids(&self.drive, volume_id).await
    }

    /// UIDs of photos and albums shared with the user, including shared albums
    /// that the shared-with-me endpoint does not yet return.
    pub async fn enumerate_shared_with_me_node_uids(&self) -> anyhow::Result<Vec<NodeUid>> {
        let mut uids = SharingOperations::enumerate_shared_with_me_node_uids(
            &self.drive,
            crate::api::share::ShareTargetType::PHOTOS,
        )
        .await?;
        uids.extend(self.enumerate_shared_with_me_album_uids().await?);
        Ok(uids)
    }

    async fn enumerate_shared_with_me_album_uids(&self) -> anyhow::Result<Vec<NodeUid>> {
        let mut uids = Vec::new();
        let mut anchor = None;
        loop {
            let response = self.photos_api.get_shared_albums(anchor).await?;
            for album in &response.albums {
                uids.push(NodeUid::new(album.volume_id.clone(), album.link_id.clone()));
            }
            if !response.more || response.anchor_id.is_none() {
                break;
            }
            anchor = response.anchor_id.clone();
        }
        Ok(uids)
    }

    /// Leaves a node that was shared with the current user.
    pub async fn leave_shared_node(&self, node_uid: NodeUid) -> anyhow::Result<()> {
        SharingOperations::leave_shared_node(&self.drive, node_uid).await
    }

    pub async fn iterate_invitations(
        &self,
    ) -> anyhow::Result<Vec<crate::sharing::ProtonInvitationWithNode>> {
        crate::sharing::SharingOperations::iterate_invitations(
            &self.drive,
            crate::api::share::ShareTargetType::PHOTOS,
        )
        .await
    }

    pub async fn accept_invitation(&self, invitation_uid: &str) -> anyhow::Result<()> {
        crate::sharing::SharingOperations::accept_invitation(&self.drive, invitation_uid).await
    }

    pub async fn reject_invitation(&self, invitation_uid: &str) -> anyhow::Result<()> {
        crate::sharing::SharingOperations::reject_invitation(&self.drive, invitation_uid).await
    }

    pub async fn share_node(
        &self,
        node_uid: NodeUid,
        settings: crate::sharing::ShareNodeSettings,
    ) -> anyhow::Result<crate::sharing::ShareResult> {
        crate::sharing::SharingOperations::share_node(&self.drive, node_uid, settings).await
    }

    pub async fn unshare_node(
        &self,
        node_uid: NodeUid,
        settings: crate::sharing::UnshareNodeSettings,
    ) -> anyhow::Result<Option<crate::sharing::ShareResult>> {
        crate::sharing::SharingOperations::unshare_node(&self.drive, node_uid, settings).await
    }

    pub async fn get_sharing_info(
        &self,
        node_uid: NodeUid,
    ) -> anyhow::Result<Option<crate::sharing::ShareResult>> {
        crate::sharing::SharingOperations::get_sharing_info(&self.drive, node_uid).await
    }

    pub async fn subscribe_to_tree_events(
        &self,
        volume_id: VolumeId,
        callback: std::sync::Arc<dyn Fn(crate::events::DriveEvent) + Send + Sync>,
    ) -> anyhow::Result<crate::events::EventSubscription> {
        self.drive.subscribe_to_tree_events(volume_id, callback).await
    }

    pub async fn subscribe_to_drive_events(
        &self,
        callback: std::sync::Arc<dyn Fn(crate::events::DriveEvent) + Send + Sync>,
    ) -> anyhow::Result<crate::events::EventSubscription> {
        self.drive.subscribe_to_drive_events(callback).await
    }

    /// Adds or removes the Favorite tag on a photo.
    pub async fn favorite_photo(&self, uid: NodeUid, favorite: bool) -> anyhow::Result<()> {
        let update = PhotoTagUpdate {
            uid,
            tags_to_add: if favorite {
                vec![PhotoTag::Favorite]
            } else {
                vec![]
            },
            tags_to_remove: if favorite {
                vec![]
            } else {
                vec![PhotoTag::Favorite]
            },
        };
        self.update_photos(vec![update]).await
    }
}
