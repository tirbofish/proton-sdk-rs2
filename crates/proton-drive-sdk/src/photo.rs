use crate::api::events::{CoreEventsResponse, VolumeEventsResponse};
use crate::api::file::photos::{
    DefaultPhotosApiClient, PhotosApiClient, PhotosApiClients, TimelinePhotoListRequest,
};
use crate::api::{DriveApiClients, DriveApiClientsFactory};
use crate::cache::entity::{DefaultPhotosEntityCache, PhotosEntityCache};
use crate::client::{ProtonDriveClient, ProtonDriveDefaults};
use crate::links::LinkId;
use crate::node::file::download::FileDownloader;
use crate::node::file::upload::FileUploader;
use crate::node::folder::FolderNode;
use crate::node::photo::{PhotosFileUploadMetadata, PhotosTimelineItem};
use crate::node::{DegradedNode, Node, NodeAndSecrets, NodeUid};
use crate::share_ops::ShareOperations;
use crate::node::DtoToMetadataConverter;
use crate::utils::PotentialObject;
use crate::volume::VolumeId;
use proton_sdk_rs2::auth::TokenCredential;
use proton_sdk_rs2::session::ProtonAPISession;
use std::sync::Arc;

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

        Ok(Self { drive, photos_api, photos_entities })
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
                metadata.base.last_modification_time
                    .map(|dt| std::time::SystemTime::from(dt)),
                metadata.base.additional_metadata,
                None, // media_info
                false, // override_existing_draft_by_other_client
            )
            .await
    }

    /// Creates a downloader for the active revision of a photo or file node.
    pub async fn get_file_downloader(
        &self,
        uid: NodeUid,
    ) -> anyhow::Result<FileDownloader> {
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
    pub async fn enumerate_trash(
        &self,
    ) -> anyhow::Result<Vec<Result<Node, DegradedNode>>> {
        self.drive.enumerate_trash().await
    }

    /// Returns the latest event-ID for the Photos volume. Use this as the
    /// starting cursor for [`poll_volume_events`].
    pub async fn get_volume_latest_event_id(
        &self,
        volume_id: VolumeId,
    ) -> anyhow::Result<String> {
        self.drive.api().events().get_volume_latest_event_id(volume_id).await
    }

    /// Poll for changes since `event_id`. Returns the next cursor and any node
    /// change events to be applied to the local cache.
    pub async fn poll_volume_events(
        &self,
        volume_id: VolumeId,
        event_id: &str,
    ) -> anyhow::Result<VolumeEventsResponse> {
        self.drive.api().events().get_volume_events(volume_id, event_id).await
    }

    /// Returns the latest global core event-ID.
    pub async fn get_core_latest_event_id(&self) -> anyhow::Result<String> {
        self.drive.api().events().get_core_latest_event_id().await
    }

    /// Poll for core-level events since `event_id`.
    pub async fn poll_core_events(
        &self,
        event_id: &str,
    ) -> anyhow::Result<CoreEventsResponse> {
        self.drive.api().events().get_core_events(event_id).await
    }

    /// Access the underlying [`ProtonDriveClient`] for operations not yet
    /// surfaced by [`ProtonPhotosClient`] (e.g. block-level upload/download).
    pub fn drive(&self) -> &ProtonDriveClient {
        &self.drive
    }
}