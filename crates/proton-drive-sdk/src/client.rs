use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;

use crate::account::{AccountClient, AccountClientAdapter};
use crate::api::DriveApiClients;
use crate::api::devices::DeviceType;
use crate::api::events::{CoreEventsResponse, VolumeEventsResponse};
use crate::api::{DefaultDriveApiClientsFactory, DriveApiClientsFactory};
use crate::block::download::BlockDownloader;
use crate::block::upload::BlockUploader;
use crate::block::verify::{BlockVerifierFactory, DefaultBlockVerifierFactory};
use crate::cache::client::{DefaultDriveClientCache, DriveClientCache};
use crate::cache::entity::{DefaultDriveEntityCache, DriveEntityCache};
use crate::cache::secret::{DefaultDriveSecretCache, DriveSecretCache};
use crate::device_ops::{Device, DeviceOperations};
use crate::links::LinkId;
use crate::meta::AdditionalMetadataProperty;
use crate::node::draft::{NewFileDraftProvider, NewRevisionDraftProvider, RevisionDraftProvider};
use crate::node::file::FileOperations;
use crate::node::file::FileThumbnail;
use crate::node::file::download::FileDownloader;
use crate::node::file::upload::FileUploader;
use crate::node::folder::{FolderNode, FolderOperations};
use crate::node::operations::NodeOperations;
use crate::node::revision::{
    REVISION_WRITER_DEFAULT_BLOCK_SIZE, RevisionInfo, RevisionState, RevisionUid,
};
use crate::node::thumbnail::ThumbnailType;
use crate::node::{DegradedNode, Node, NodeUid};
use crate::utils::PotentialObject;
use crate::utils::semaphore::FifoFlexibleSemaphore;
use crate::volume::VolumeId;
use crate::volume_operations::VolumeOperations;
use proton_sdk_rs2::auth::TokenCredential;
use proton_sdk_rs2::client::ProtonApiDefaults;
use proton_sdk_rs2::{
    client::{FeatureFlagProvider, Telemetry},
    session::ProtonAPISession,
};

pub struct ProtonDriveDefaults;

impl ProtonDriveDefaults {
    pub const STORAGE_API_TIMEOUT_SECONDS: u32 = 300;
    pub const DRIVE_BASE_ROUTE: &str = "drive/";
}

#[derive(Debug, Clone, Default)]
pub struct ProtonDriveClientOptions {
    pub uid: Option<String>,
    /// The language of the bindings. Can be used as a potential spoof for something.
    ///
    /// By default, it is `None`, but when constructed in the x-pm-appversion, it is considered as `rust`.
    pub bindings_language: Option<String>,
    /// The amount of time in seconds before a timeout.
    pub api_call_timeout: Option<u32>,
    /// The amount of time in seconds before a timeout for storage-based api's.
    pub storage_call_timeout: Option<u32>,
}

#[derive(Clone)]
pub struct ProtonDriveClient {
    uid: String,
    account: Arc<dyn AccountClient>,
    api: Arc<dyn DriveApiClients>,
    cache: Arc<dyn DriveClientCache>,
    block_verifier_factory: Arc<dyn BlockVerifierFactory>,
    telemetry: Arc<dyn Telemetry>,
    feature_flag_provider: Arc<dyn FeatureFlagProvider>,
    revision_creation_semaphore: FifoFlexibleSemaphore,
    block_listing_semaphore: FifoFlexibleSemaphore,
    target_block_size: usize,
    block_uploader: BlockUploader,
    block_downloader: BlockDownloader,
    thumbnail_block_downloader: BlockDownloader,
    sdk_events: Arc<crate::events::SdkEvents>,
}

// initialisers
impl ProtonDriveClient {
    const DEFAULT_DEGREE_OF_BLOCK_TRANSFER_PARALLELISM: usize = 6;
    const MAX_DEGREE_OF_THUMBNAIL_DOWNLOAD_PARALLELISM: usize = 8;

    /// Creates a new [`ProtonDriveClient`] based on an existing [`ProtonAPISession`].
    ///
    /// The defacto initialiser.
    ///
    /// The `uid` is an optional unique identifier for this client instance, useful for logging
    /// and debugging. If `None`, a unique ID is auto-generated using the current timestamp.
    pub fn new(session: &ProtonAPISession, uid: Option<String>) -> anyhow::Result<Self> {
        Self::from_session_with_drive_api_clients_factory(
            session,
            Arc::new(DefaultDriveApiClientsFactory),
            uid,
        )
    }

    /// Creates a new [`ProtonDriveClient`] by ensuring that the session is authenticated.
    ///
    /// Can throw an error if any issues occur with authentication.
    ///
    /// The `uid` is an optional unique identifier for this client instance, useful for logging
    /// and debugging. If `None`, a unique ID is auto-generated using the current timestamp.
    pub async fn new_with_preflight_auth(
        session: &mut ProtonAPISession,
        uid: Option<String>,
    ) -> anyhow::Result<Self> {
        session.ensure_authenticated().await?;
        Self::from_session_with_drive_api_clients_factory(
            session,
            Arc::new(DefaultDriveApiClientsFactory),
            uid,
        )
    }

    /// Creates a new [`ProtonDriveClient`] from custom implementations of clients and caches.
    ///
    /// Use this if you want full control, however this is typically derived
    /// from [`ProtonAPISession`] in [`Self::new`]
    pub fn from_http_client_factory(
        account_client: Arc<dyn AccountClient>,
        entity_cache_repository: Arc<dyn DriveEntityCache>,
        secret_cache_repository: Arc<dyn DriveSecretCache>,
        feature_flag_provider: Arc<dyn FeatureFlagProvider>,
        telemetry: Arc<dyn Telemetry>,
        creation_parameters: Option<ProtonDriveClientOptions>,
    ) -> anyhow::Result<Self> {
        Self::from_http_client_factory_with_drive_api_clients_factory(
            account_client,
            entity_cache_repository,
            secret_cache_repository,
            feature_flag_provider,
            telemetry,
            Arc::new(DefaultDriveApiClientsFactory),
            creation_parameters,
        )
    }

    pub(crate) fn from_session_with_drive_api_clients_factory(
        session: &ProtonAPISession,
        drive_api_clients_factory: Arc<dyn DriveApiClientsFactory>,
        uid: Option<String>,
    ) -> anyhow::Result<Self> {
        let default_api_http_client = session.get_http_client(
            Some(ProtonDriveDefaults::DRIVE_BASE_ROUTE.to_string()),
            Some(std::time::Duration::from_secs(
                ProtonApiDefaults::DEFAULT_TIMEOUT_SECONDS as u64,
            )),
            None,
        )?;

        let storage_api_http_client = session.get_http_client(
            Some(ProtonDriveDefaults::DRIVE_BASE_ROUTE.to_string()),
            Some(std::time::Duration::from_secs(
                ProtonDriveDefaults::STORAGE_API_TIMEOUT_SECONDS as u64,
            )),
            Some(std::time::Duration::from_secs(
                ProtonDriveDefaults::STORAGE_API_TIMEOUT_SECONDS as u64,
            )),
        )?;

        let cache: Arc<dyn DriveClientCache> = Arc::new(DefaultDriveClientCache::new(
            Arc::new(DefaultDriveEntityCache::new(
                session.client_config.entity_cache_repository.clone(),
            )) as Arc<dyn DriveEntityCache>,
            Arc::new(DefaultDriveSecretCache::new(
                session.client_config.secret_cache_repository.clone(),
            )) as Arc<dyn DriveSecretCache>,
        ));

        let default_api_base_address =
            reqwest::Url::parse(&session.client_config.base_url.to_string())?
                .join(ProtonDriveDefaults::DRIVE_BASE_ROUTE)?;

        let storage_api_base_address =
            reqwest::Url::parse(&session.client_config.base_url.to_string())?
                .join(ProtonDriveDefaults::DRIVE_BASE_ROUTE)?;

        Self::from_http_clients(
            reqwest_middleware::ClientBuilder::new(default_api_http_client).build(),
            reqwest_middleware::ClientBuilder::new(storage_api_http_client).build(),
            default_api_base_address,
            storage_api_base_address,
            Arc::new(AccountClientAdapter::new(session)) as Arc<dyn AccountClient>,
            cache,
            session.client_config.feature_flag_provider.clone(),
            session.client_config.telemetry.clone(),
            drive_api_clients_factory,
            uid.unwrap_or_else(generate_uid),
            Some(session.token_credential.clone()),
            session.client_config.base_url.to_string(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_http_clients(
        default_api_http_client: reqwest_middleware::ClientWithMiddleware,
        storage_api_http_client: reqwest_middleware::ClientWithMiddleware,
        default_api_base_url: reqwest::Url,
        storage_api_base_url: reqwest::Url,
        account_client: Arc<dyn AccountClient>,
        cache: Arc<dyn DriveClientCache>,
        feature_flag_provider: Arc<dyn FeatureFlagProvider>,
        telemetry: Arc<dyn Telemetry>,
        drive_api_clients_factory: Arc<dyn DriveApiClientsFactory>,
        uid: String,
        token_credential: Option<TokenCredential>,
        _api_url: String,
    ) -> anyhow::Result<Self> {
        let api = drive_api_clients_factory.create(
            default_api_http_client.clone(),
            storage_api_http_client.clone(),
            default_api_base_url.clone(),
            storage_api_base_url,
            token_credential.clone(),
        );

        Ok(Self::from_components(
            account_client,
            api,
            cache,
            Arc::new(DefaultBlockVerifierFactory::new(
                default_api_http_client,
                default_api_base_url,
                token_credential,
            )) as Arc<dyn BlockVerifierFactory>,
            feature_flag_provider,
            telemetry,
            uid,
            None,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_http_client_factory_with_drive_api_clients_factory(
        account_client: Arc<dyn AccountClient>,
        entity_cache_repository: Arc<dyn DriveEntityCache>,
        secret_cache_repository: Arc<dyn DriveSecretCache>,
        feature_flag_provider: Arc<dyn FeatureFlagProvider>,
        telemetry: Arc<dyn Telemetry>,
        drive_api_clients_factory: Arc<dyn DriveApiClientsFactory>,
        creation_parameters: Option<ProtonDriveClientOptions>,
    ) -> anyhow::Result<Self> {
        let options = creation_parameters.unwrap_or_default();
        let default_api_timeout = options
            .api_call_timeout
            .unwrap_or(ProtonApiDefaults::DEFAULT_TIMEOUT_SECONDS);
        let storage_api_timeout = options
            .storage_call_timeout
            .unwrap_or(ProtonDriveDefaults::STORAGE_API_TIMEOUT_SECONDS);

        let default_api_http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(default_api_timeout as u64))
            .build()?;
        let storage_api_http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(storage_api_timeout as u64))
            .build()?;

        let cache: Arc<dyn DriveClientCache> = Arc::new(DefaultDriveClientCache::new(
            entity_cache_repository.clone() as Arc<dyn DriveEntityCache>,
            secret_cache_repository.clone() as Arc<dyn DriveSecretCache>,
        ));

        let api_base_url = "https://drive-api.proton.me/".to_string();
        let base_address =
            reqwest::Url::parse(&api_base_url)?.join(ProtonDriveDefaults::DRIVE_BASE_ROUTE)?;

        Self::from_http_clients(
            reqwest_middleware::ClientBuilder::new(default_api_http_client).build(),
            reqwest_middleware::ClientBuilder::new(storage_api_http_client).build(),
            base_address.clone(),
            base_address,
            account_client,
            cache,
            feature_flag_provider,
            telemetry,
            drive_api_clients_factory,
            options.uid.unwrap_or_else(generate_uid),
            None,
            api_base_url,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_components(
        account_client: Arc<dyn AccountClient>,
        api: Arc<dyn DriveApiClients>,
        cache: Arc<dyn DriveClientCache>,
        block_verifier_factory: Arc<dyn BlockVerifierFactory>,
        feature_flag_provider: Arc<dyn FeatureFlagProvider>,
        telemetry: Arc<dyn Telemetry>,
        uid: String,
        block_transfer_degree_of_parallelism: Option<usize>,
    ) -> Self {
        let max_degree_of_block_transfer_parallelism =
            block_transfer_degree_of_parallelism.unwrap_or_else(default_block_transfer_parallelism);

        let max_degree_of_block_processing_parallelism = max_degree_of_block_transfer_parallelism
            + ((max_degree_of_block_transfer_parallelism / 2).clamp(2, 4));

        let revision_creation_semaphore =
            FifoFlexibleSemaphore::new(max_degree_of_block_processing_parallelism);
        let block_listing_semaphore =
            FifoFlexibleSemaphore::new(max_degree_of_block_processing_parallelism);

        let block_uploader = BlockUploader::new(max_degree_of_block_transfer_parallelism);
        let block_downloader = BlockDownloader::new(max_degree_of_block_transfer_parallelism);
        let thumbnail_block_downloader =
            BlockDownloader::new(Self::MAX_DEGREE_OF_THUMBNAIL_DOWNLOAD_PARALLELISM);

        Self {
            uid,
            account: account_client,
            api,
            cache,
            block_verifier_factory,
            telemetry,
            feature_flag_provider,
            revision_creation_semaphore,
            block_listing_semaphore,
            target_block_size: REVISION_WRITER_DEFAULT_BLOCK_SIZE,
            block_uploader,
            block_downloader,
            thumbnail_block_downloader,
            sdk_events: Arc::new(crate::events::SdkEvents::new()),
        }
    }
}

// getters
impl ProtonDriveClient {
    /// Returns the unique identifier assigned to this client instance.
    pub fn uid(&self) -> &str {
        &self.uid
    }

    /// Returns the target encrypted block size (in bytes) used when uploading file data.
    pub fn target_block_size(&self) -> usize {
        self.target_block_size
    }

    /// Overrides the target encrypted block size. `value` is in bytes; the default is 4 MiB.
    pub fn set_target_block_size(&mut self, value: usize) {
        self.target_block_size = value;
    }

    pub(crate) fn account(&self) -> &Arc<dyn AccountClient> {
        &self.account
    }

    /// Returns the Drive API client bundle used for all HTTP calls.
    pub fn api(&self) -> &Arc<dyn DriveApiClients> {
        &self.api
    }

    /// Returns the user's storage quota information (used_space, max_space) in bytes.
    pub async fn get_user_storage_info(&self) -> anyhow::Result<(i64, i64)> {
        self.account.get_user_storage_info().await
    }

    /// Lists the direct children of a folder, or the root "My Files" folder if `parent_link_id` is `None`.
    /// Fetches, decrypts and returns all items up-front, collecting the stream into a `Vec`.
    pub async fn list_children(
        &self,
        volume_id: VolumeId,
        parent_link_id: Option<LinkId>,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        let parent_uid = parent_link_id.map(|id| NodeUid::new(volume_id.clone(), id));
        let stream = match parent_uid {
            Some(uid) => self.enumerate_folder_children(uid).await?,
            None => {
                let root = self.get_my_files_folder().await?;
                self.enumerate_folder_children(root.base.uid).await?
            }
        };
        tokio::pin!(stream);
        let mut results = Vec::new();
        while let Some(item) = futures::StreamExt::next(&mut stream).await {
            results.push(item?);
        }
        Ok(results)
    }

    pub(crate) fn cache(&self) -> &Arc<dyn DriveClientCache> {
        &self.cache
    }

    pub(crate) fn block_verifier_factory(&self) -> &Arc<dyn BlockVerifierFactory> {
        &self.block_verifier_factory
    }

    /// Returns the telemetry sink attached to this client.
    pub fn telemetry(&self) -> &Arc<dyn Telemetry> {
        &self.telemetry
    }

    /// SDK-level transfer and throttle events.
    pub fn sdk_events(&self) -> &Arc<crate::events::SdkEvents> {
        &self.sdk_events
    }

    /// File and block transfer permits used by upload and download.
    pub fn transfer_queue(&self) -> &crate::node::transfer::TransferQueue {
        &self.block_uploader.queue
    }

    /// Returns the feature flag provider used to gate experimental behaviour.
    pub fn feature_flag_provider(&self) -> &Arc<dyn FeatureFlagProvider> {
        &self.feature_flag_provider
    }

    /// Returns the semaphore used to limit the number of concurrent revision creation requests.
    pub fn revision_creation_semaphore(&self) -> &FifoFlexibleSemaphore {
        &self.revision_creation_semaphore
    }

    /// Returns the semaphore used to limit concurrent block-list fetch requests.
    pub fn block_listing_semaphore(&self) -> &FifoFlexibleSemaphore {
        &self.block_listing_semaphore
    }

    /// Returns the block uploader, which manages upload parallelism.
    pub fn block_uploader(&self) -> &BlockUploader {
        &self.block_uploader
    }

    /// Returns the block downloader used for file content.
    pub fn block_downloader(&self) -> &BlockDownloader {
        &self.block_downloader
    }

    /// Returns the block downloader dedicated to thumbnail content.
    pub fn thumbnail_block_downloader(&self) -> &BlockDownloader {
        &self.thumbnail_block_downloader
    }
}

// the meat
impl ProtonDriveClient {
    /// Returns the root "My Files" folder for the authenticated user.
    pub async fn get_my_files_folder(&self) -> anyhow::Result<FolderNode> {
        NodeOperations::get_my_files_folder(self).await
    }

    /// Fetches and decrypts a single node by its `NodeUid`.
    /// Returns a `PotentialObject` that is either a fully-decrypted `Node` or a `DegradedNode` when decryption partially fails.
    pub async fn get_node(
        &self,
        node_uid: NodeUid,
    ) -> anyhow::Result<PotentialObject<Node, DegradedNode>> {
        NodeOperations::get_node(self, node_uid).await
    }

    /// Like `get_node`, but bypasses the entity cache and always fetches fresh data from the API.
    /// Use this when you know the cached node metadata may be stale (e.g. after uploading a new revision).
    pub async fn get_node_uncached(
        &self,
        node_uid: NodeUid,
    ) -> anyhow::Result<PotentialObject<Node, DegradedNode>> {
        let _ = self.cache().entities().remove_node(node_uid.clone()).await;
        NodeOperations::get_node(self, node_uid).await
    }

    /// Fetch and decrypt a thumbnail block belonging to the file identified by
    /// `node_uid`.  `thumbnail_id` is the server-assigned ID stored in the
    /// node's `Revision.thumbnails` list.  Returns the raw (decrypted) image
    /// bytes on success, which you will have to construct yourself with a crate like `image`.
    pub async fn fetch_thumbnail(
        &self,
        node_uid: NodeUid,
        thumbnail_id: String,
    ) -> anyhow::Result<Vec<u8>> {
        let secrets = FileOperations::get_secrets(self, node_uid.clone())
            .await
            .context("Failed to get file secrets for thumbnail")?;
        let volume_id = node_uid.volume_id.clone();

        let resp = self
            .api()
            .files()
            .get_thumbnail_blocks(volume_id, vec![thumbnail_id.clone()])
            .await
            .context("Failed to get thumbnail block info from server")?;

        tracing::debug!(
            "Looking for thumbnail_id='{}' in {} blocks: {:?}",
            thumbnail_id,
            resp.blocks.len(),
            resp.blocks
                .iter()
                .map(|b| b.thumbnail_id.as_str())
                .collect::<Vec<_>>()
        );

        let available_ids: Vec<String> =
            resp.blocks.iter().map(|b| b.thumbnail_id.clone()).collect();
        let block = resp
            .blocks
            .into_iter()
            .find(|b| b.thumbnail_id == thumbnail_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "thumbnail block not returned by server: requested '{}' but got {:?}",
                    thumbnail_id,
                    available_ids
                )
            })?;

        tracing::debug!(
            "Downloading thumbnail from {} (token len={})",
            block.bare_url,
            block.token.len()
        );

        let response = self
            .api()
            .storage()
            .get_blob_stream(&block.bare_url, &block.token)
            .await
            .context("Failed to initiate thumbnail blob download")?;

        let content_length = response.content_length();
        tracing::debug!(
            "Thumbnail response content_length={:?}, status={}",
            content_length,
            response.status()
        );

        let blob_bytes = response
            .bytes()
            .await
            .context("Failed to read thumbnail blob bytes")?;

        tracing::debug!("Read {} bytes from thumbnail blob", blob_bytes.len());

        secrets
            .content_key
            .decrypt(&blob_bytes)
            .context("Failed to decrypt thumbnail")
    }

    /// Fetches and decrypts multiple nodes in parallel, returning results in an unordered stream.
    pub async fn enumerate_nodes(
        &self,
        node_uids: Vec<NodeUid>,
    ) -> anyhow::Result<
        impl futures::Stream<Item = anyhow::Result<PotentialObject<Node, DegradedNode>>> + '_,
    > {
        // enumerate_nodes now uses FuturesUnordered internally for parallelism.
        let results = NodeOperations::enumerate_nodes(self, node_uids).await?;
        Ok(futures::stream::iter(results.into_iter().map(Ok)))
    }

    /// Creates a new folder under `parent_id` with the given `name`.
    /// `last_modification_time` sets the folder's extended-attributes modification timestamp.
    pub async fn create_folder(
        &self,
        parent_id: NodeUid,
        name: String,
        last_modification_time: Option<std::time::SystemTime>,
    ) -> anyhow::Result<FolderNode> {
        FolderOperations::create(self, parent_id, name, last_modification_time).await
    }

    /// Enumerate children of a folder, streaming items as they are fetched and
    /// decrypted.  The `.await` returns immediately after spawning the background
    /// task; items appear in the stream as soon as each batch is ready.
    pub async fn enumerate_folder_children(
        &self,
        folder_id: impl Into<NodeUid>,
    ) -> anyhow::Result<
        impl futures::Stream<Item = anyhow::Result<PotentialObject<Node, DegradedNode>>> + 'static,
    > {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let client = self.clone();
        tokio::spawn(FolderOperations::enumerate_children_to_channel(
            client,
            folder_id.into(),
            tx,
        ));
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }

    /// Fetches thumbnails for the given files, filtered by `thumbnail_type`.
    /// Returns an unordered stream of `FileThumbnail` items.
    pub async fn enumerate_thumbnails(
        &self,
        file_uids: Vec<NodeUid>,
        thumbnail_type: ThumbnailType,
    ) -> anyhow::Result<impl futures::Stream<Item = anyhow::Result<FileThumbnail>> + '_> {
        let results = FileOperations::enumerate_thumbnails(self, file_uids, thumbnail_type).await?;
        Ok(futures::stream::iter(results.into_iter().map(Ok)))
    }

    /// Creates a `FileUploader` for a brand-new file under `parent_folder_uid`.
    /// Set `override_existing_draft_by_other_client` to `true` to replace an abandoned draft from another client.
    pub async fn get_file_uploader(
        &self,
        parent_folder_uid: NodeUid,
        name: String,
        media_type: String,
        size: i64,
        last_modification_time: Option<std::time::SystemTime>,
        additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
        media_info: Option<crate::api::attr::MediaExtendedAttributes>,
        override_existing_draft_by_other_client: bool,
    ) -> anyhow::Result<FileUploader> {
        let draft_provider = NewFileDraftProvider {
            client: Arc::new(self.clone()),
            parent_folder_uid,
            name,
            media_type,
            override_existing_draft_by_other_client,
        };

        self.get_file_uploader_from_draft_provider(
            Box::new(draft_provider),
            size,
            last_modification_time,
            additional_metadata,
            media_info,
        )
        .await
    }

    /// Creates a `FileUploader` for a new revision of an existing file.
    /// Pass the `RevisionUid` of the current active revision to replace it.
    pub async fn get_file_revision_uploader(
        &self,
        current_active_revision_uid: RevisionUid,
        size: i64,
        last_modification_time: Option<std::time::SystemTime>,
        additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
        media_info: Option<crate::api::attr::MediaExtendedAttributes>,
    ) -> anyhow::Result<FileUploader> {
        let draft_provider = NewRevisionDraftProvider {
            client: Arc::new(self.clone()),
            node_uid: current_active_revision_uid.node_uid,
            revision_id: current_active_revision_uid.revision_id,
        };

        self.get_file_uploader_from_draft_provider(
            Box::new(draft_provider),
            size,
            last_modification_time,
            additional_metadata,
            media_info,
        )
        .await
    }

    /// Creates a `FileDownloader` for the active revision of a file, identified by its `RevisionUid`.
    pub async fn get_file_downloader(
        &self,
        revision_uid: RevisionUid,
    ) -> anyhow::Result<FileDownloader> {
        FileDownloader::create(self, revision_uid).await
    }

    /// Creates a `FileDownloader` for a specific revision identified by its `RevisionUid`.
    /// Use this to download a historical revision rather than the currently active one.
    pub async fn get_file_revision_downloader(
        &self,
        revision_uid: RevisionUid,
    ) -> anyhow::Result<FileDownloader> {
        FileDownloader::create(self, revision_uid).await
    }

    /// Returns all non-draft revisions for the given file, decrypting extended attributes
    /// (size, modification time, SHA1 digest) with the node key where possible.
    pub async fn iterate_revisions(&self, node_uid: NodeUid) -> anyhow::Result<Vec<RevisionInfo>> {
        use crate::api::attr::ExtendedAttributes;
        use crate::author::Author;
        use crate::node::authorship::AuthorshipClaim;
        use crate::node::crypto::NodeCrypto;

        let secrets = FileOperations::get_secrets(self, node_uid.clone()).await?;
        let node_key = secrets.base.key;

        let resp = self
            .api()
            .files()
            .get_revisions(node_uid.volume_id.clone(), node_uid.link_id.clone())
            .await?;

        let authorship_claim = AuthorshipClaim {
            keys: vec![],
            author: Author::ANONYMOUS,
            key_retrieval_error_message: None,
        };

        let mut results = Vec::new();
        for dto in resp.revisions {
            if dto.state == RevisionState::Draft {
                continue;
            }

            let (claimed_size, claimed_modification_time, claimed_sha1) = if let Some(xattr_msg) =
                &dto.extended_attributes
            {
                match NodeCrypto::decrypt_message(xattr_msg, None, [&node_key], &authorship_claim) {
                    Ok((bytes, _, _)) => {
                        if let Ok(xattr) = serde_json::from_slice::<ExtendedAttributes>(&bytes) {
                            let common = xattr.common.as_ref();
                            (
                                common.and_then(|c| c.size),
                                common.and_then(|c| c.modification_time),
                                common
                                    .and_then(|c| c.digests.as_ref())
                                    .and_then(|d| d.sha1.clone()),
                            )
                        } else {
                            (None, None, None)
                        }
                    }
                    Err(_) => (None, None, None),
                }
            } else {
                (None, None, None)
            };

            results.push(RevisionInfo {
                uid: RevisionUid::new(node_uid.clone(), dto.id),
                state: dto.state,
                creation_time: dto.creation_time,
                size_on_cloud_storage: dto.size,
                claimed_size,
                claimed_modification_time,
                claimed_sha1,
            });
        }

        Ok(results)
    }

    /// Restores the given revision as the active revision of its file.
    /// This operation promotes a superseded revision back to active.
    pub async fn restore_revision(&self, revision_uid: RevisionUid) -> anyhow::Result<()> {
        let (node_uid, revision_id) = revision_uid.deconstruct();
        let resp = self
            .api()
            .files()
            .restore_revision(node_uid.volume_id, node_uid.link_id, revision_id)
            .await?;
        if !resp.is_success() {
            anyhow::bail!(
                "restore_revision failed: code={} error={:?}",
                resp.code.0,
                resp.error_message
            );
        }
        Ok(())
    }

    /// Permanently deletes the given revision. Only superseded revisions can be deleted.
    /// Active revisions should be fully deleted by deleting the parent file node instead.
    pub async fn delete_revision(&self, revision_uid: RevisionUid) -> anyhow::Result<()> {
        let (node_uid, revision_id) = revision_uid.deconstruct();
        let resp = self
            .api()
            .files()
            .delete_revision(node_uid.volume_id, node_uid.link_id, revision_id)
            .await?;
        if !resp.is_success() {
            anyhow::bail!(
                "delete_revision failed: code={} error={:?}",
                resp.code.0,
                resp.error_message
            );
        }
        Ok(())
    }

    /// Deletes any draft revisions for the given file.
    /// This is useful for cleaning up abandoned drafts before creating a new revision.
    /// Returns the number of drafts deleted.
    pub async fn delete_draft_revisions(&self, node_uid: NodeUid) -> anyhow::Result<usize> {
        let resp = self
            .api()
            .files()
            .get_revisions(node_uid.volume_id.clone(), node_uid.link_id.clone())
            .await?;

        let mut deleted = 0;
        for dto in resp.revisions {
            if dto.state == RevisionState::Draft {
                let revision_uid = RevisionUid::new(node_uid.clone(), dto.id);
                match self.delete_revision(revision_uid).await {
                    Ok(()) => {
                        deleted += 1;
                        tracing::info!("Deleted draft revision for {:?}", node_uid);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to delete draft revision: {}", e);
                    }
                }
            }
        }

        Ok(deleted)
    }

    /// Downloads the active revision of a file node and writes it to `path` on disk.
    /// `on_progress` is called with (bytes_written, total_bytes) as data arrives.
    pub async fn download_to_file(
        &self,
        node_uid: NodeUid,
        path: &std::path::Path,
        on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> anyhow::Result<()> {
        let potential_node = self.get_node(node_uid).await?;
        let node = potential_node
            .result()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let file = match node {
            crate::node::Node::File(f) => f,
            _ => anyhow::bail!("Expected file node"),
        };

        let downloader = self
            .get_file_downloader(file.active_revision.uid.clone())
            .await?;
        let file = std::fs::File::create(path)?;
        let controller = downloader.download_to_stream(Box::new(file), on_progress);
        controller.completion.await?
    }

    /// Reads `path` from disk and uploads it as a new file under `parent_folder_uid`.
    /// Set `override_existing` to `true` to replace an existing file with the same name.
    pub async fn upload_file(
        &self,
        path: &std::path::Path,
        parent_folder_uid: NodeUid,
        override_existing: bool,
        on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> anyhow::Result<NodeUid> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid file name"))?
            .to_string();
        let metadata = std::fs::metadata(path)?;
        let size = metadata.len() as i64;
        let last_modified = metadata.modified().ok();

        let media_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();

        let file_data = std::fs::read(path)?;
        #[cfg(feature = "thumbnail-generation")]
        let (thumbnails, media_info) = if media_type.starts_with("image/") {
            crate::utils::thumbnail::ThumbnailGenerator::generate_thumbnails(&file_data)
        } else {
            (Vec::new(), None)
        };
        #[cfg(not(feature = "thumbnail-generation"))]
        let (thumbnails, media_info): (
            Vec<crate::node::thumbnail::Thumbnail>,
            Option<crate::api::attr::MediaExtendedAttributes>,
        ) = (Vec::new(), None);

        let uploader = self
            .get_file_uploader(
                parent_folder_uid,
                name,
                media_type,
                size,
                last_modified,
                None,
                media_info,
                override_existing,
            )
            .await?;

        uploader
            .upload_from_stream(
                Box::new(std::io::Cursor::new(file_data)),
                thumbnails,
                on_progress,
            )
            .await
    }

    /// Returns a unique variant of `name` that does not conflict with any existing child under `parent_uid`.
    /// If `name` is already available it is returned unchanged.
    pub async fn get_available_name(
        &self,
        parent_uid: NodeUid,
        name: String,
    ) -> anyhow::Result<String> {
        NodeOperations::get_available_name(self, parent_uid, name).await
    }

    /// Moves multiple nodes to `new_parent_folder_uid`, re-encrypting their key material for the new parent.
    pub async fn move_nodes(
        &self,
        uids: Vec<NodeUid>,
        new_parent_folder_uid: NodeUid,
    ) -> anyhow::Result<()> {
        NodeOperations::move_multiple(self, uids, new_parent_folder_uid).await
    }

    /// Copies a single node to `new_parent_folder_uid`, optionally renaming it with `new_name`.
    /// Returns the `LinkId` of the newly created copy.
    pub async fn copy_node(
        &self,
        uid: NodeUid,
        new_parent_folder_uid: NodeUid,
        new_name: Option<String>,
    ) -> anyhow::Result<crate::links::LinkId> {
        NodeOperations::copy_single(self, uid, new_parent_folder_uid, new_name).await
    }

    /// Renames a node, optionally updating its `new_media_type` MIME type.
    pub async fn rename_node(
        &self,
        uid: NodeUid,
        new_name: String,
        new_media_type: Option<String>,
    ) -> anyhow::Result<()> {
        NodeOperations::rename(self, uid, new_name, new_media_type).await
    }

    /// Moves the given nodes to the trash. Returns a per-node result map.
    pub async fn trash_nodes(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        NodeOperations::trash(self, uids).await
    }

    /// Permanently deletes nodes that are currently in the default location (not from trash). Returns a per-node result map.
    pub async fn delete_nodes(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        NodeOperations::delete(self, uids).await
    }

    /// Permanently deletes nodes from the trash. Returns a per-node result map.
    pub async fn delete_nodes_from_trash(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        NodeOperations::delete_from_trash(self, uids).await
    }

    /// Restores previously-trashed nodes to their original location. Returns a per-node result map.
    pub async fn restore_nodes(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        NodeOperations::restore(self, uids).await
    }

    /// Returns all nodes currently in the trash, decrypting each where possible.
    pub async fn enumerate_trash(&self) -> anyhow::Result<Vec<Result<Node, DegradedNode>>> {
        VolumeOperations::enumerate_trash(self).await
    }

    /// Streams trash items progressively as they are fetched and decrypted,
    /// rather than waiting for all pages. Items arrive batch-by-batch.
    pub fn stream_trash(
        &self,
    ) -> impl futures::Stream<Item = anyhow::Result<Result<Node, DegradedNode>>> + 'static {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let client = self.clone();
        tokio::spawn(VolumeOperations::enumerate_trash_to_channel(client, tx));
        futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
    }

    /// Permanently deletes every item in the trash for this user.
    pub async fn empty_trash(&self) -> anyhow::Result<()> {
        VolumeOperations::empty_trash(self).await
    }

    /// Returns the latest known event-ID for the given volume. Use this cursor
    /// to start polling with [`poll_volume_events`].
    pub async fn get_volume_latest_event_id(&self, volume_id: VolumeId) -> anyhow::Result<String> {
        self.api()
            .events()
            .get_volume_latest_event_id(volume_id)
            .await
    }

    /// Polls for volume events since `event_id`. The [`VolumeEventsResponse`]
    /// contains the next cursor (`event_id`), whether there are more pages
    /// (`more`), whether a full refresh is required (`refresh`), and the list
    /// of raw [`VolumeEventDto`] items.
    pub async fn poll_volume_events(
        &self,
        volume_id: VolumeId,
        event_id: &str,
    ) -> anyhow::Result<VolumeEventsResponse> {
        self.api()
            .events()
            .get_volume_events(volume_id, event_id)
            .await
    }

    /// Returns the latest known global core event-ID.
    pub async fn get_core_latest_event_id(&self) -> anyhow::Result<String> {
        self.api().events().get_core_latest_event_id().await
    }

    /// Polls for core-level events since `event_id`.
    pub async fn poll_core_events(&self, event_id: &str) -> anyhow::Result<CoreEventsResponse> {
        self.api().events().get_core_events(event_id).await
    }

    /// Returns all Computers (backup devices) registered for this account.
    pub async fn list_devices(&self) -> anyhow::Result<Vec<Device>> {
        DeviceOperations::list_devices(self).await
    }

    /// Returns a single Computer by its device ID, decrypting its name.
    pub async fn get_device(&self, device_id: &str) -> anyhow::Result<Device> {
        DeviceOperations::get_device(self, device_id).await
    }

    /// Creates a new Computer entry with the given display name and device type.
    pub async fn create_device(
        &self,
        name: String,
        device_type: DeviceType,
    ) -> anyhow::Result<Device> {
        DeviceOperations::create_device(self, name, device_type).await
    }

    /// Renames an existing Computer.
    pub async fn rename_device(&self, device_id: &str, new_name: String) -> anyhow::Result<Device> {
        DeviceOperations::rename_device(self, device_id, new_name).await
    }

    /// Unregisters a Computer, removing it from the account.
    pub async fn delete_device(&self, device_id: &str) -> anyhow::Result<()> {
        DeviceOperations::delete_device(self, device_id).await
    }

    /// Builds a node UID from raw volume and link IDs (`volumeId~linkId`).
    pub fn generate_node_uid(volume_id: impl Into<String>, node_id: impl Into<String>) -> NodeUid {
        NodeUid::from_parts(volume_id, node_id)
    }

    /// Proton Drive (or Docs) web URL for opening this node in the production web app.
    pub async fn get_node_url(&self, node_uid: NodeUid) -> anyhow::Result<String> {
        let node = self
            .get_node(node_uid.clone())
            .await?
            .result()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let media_type = match &node {
            Node::File(f) | Node::Photo(f) => Some(f.base.media_type.as_str()),
            Node::Folder(_) | Node::Album(_) => None,
        };
        let is_file = matches!(node.ty(), crate::node::NodeType::File | crate::node::NodeType::Photo);
        if crate::node::is_proton_document(media_type) || crate::node::is_proton_sheet(media_type) {
            return Ok(crate::node::node_web_url("", &node_uid, is_file, media_type));
        }
        let context = self
            .api()
            .links()
            .get_context_share(node_uid.volume_id.clone(), node_uid.link_id.clone())
            .await?;
        Ok(crate::node::node_web_url(
            context.context_share_id.raw(),
            &node_uid,
            is_file,
            media_type,
        ))
    }

    /// Content session key used by Proton Docs to encrypt and decrypt document updates.
    pub async fn get_docs_key(&self, node_uid: NodeUid) -> anyhow::Result<crate::pgp::PgpSessionKey> {
        Ok(FileOperations::get_secrets(self, node_uid).await?.content_key)
    }

    /// UIDs of nodes the user has shared from My Files.
    pub async fn enumerate_shared_node_uids(&self) -> anyhow::Result<Vec<NodeUid>> {
        let my_files = self.get_my_files_folder().await?;
        crate::share_ops::SharingOperations::enumerate_shared_node_uids(
            self,
            my_files.base.uid.volume_id,
        )
        .await
    }

    /// UIDs of nodes shared with the user (files, folders, and Proton Docs).
    pub async fn enumerate_shared_with_me_node_uids(&self) -> anyhow::Result<Vec<NodeUid>> {
        crate::share_ops::SharingOperations::enumerate_shared_with_me_node_uids(
            self,
            crate::api::share::ShareTargetType::DRIVE,
        )
        .await
    }

    /// Leaves a node that was shared with the current user.
    pub async fn leave_shared_node(&self, node_uid: NodeUid) -> anyhow::Result<()> {
        crate::share_ops::SharingOperations::leave_shared_node(self, node_uid).await
    }

    pub async fn iterate_invitations(
        &self,
    ) -> anyhow::Result<Vec<crate::sharing::ProtonInvitationWithNode>> {
        crate::sharing::SharingOperations::iterate_invitations(
            self,
            crate::api::share::ShareTargetType::DRIVE,
        )
        .await
    }

    pub async fn accept_invitation(&self, invitation_uid: &str) -> anyhow::Result<()> {
        crate::sharing::SharingOperations::accept_invitation(self, invitation_uid).await
    }

    pub async fn reject_invitation(&self, invitation_uid: &str) -> anyhow::Result<()> {
        crate::sharing::SharingOperations::reject_invitation(self, invitation_uid).await
    }

    pub async fn resend_invitation_email(&self, invitation_uid: &str) -> anyhow::Result<()> {
        crate::sharing::SharingOperations::resend_invitation_email(self, invitation_uid).await
    }

    pub async fn convert_non_proton_invitation(
        &self,
        node_uid: NodeUid,
        invitation_uid: &str,
    ) -> anyhow::Result<crate::sharing::ProtonInvitation> {
        crate::sharing::SharingOperations::convert_non_proton_invitation(
            self,
            node_uid,
            invitation_uid,
        )
        .await
    }

    pub async fn get_sharing_info(
        &self,
        node_uid: NodeUid,
    ) -> anyhow::Result<Option<crate::sharing::ShareResult>> {
        crate::sharing::SharingOperations::get_sharing_info(self, node_uid).await
    }

    pub async fn share_node(
        &self,
        node_uid: NodeUid,
        settings: crate::sharing::ShareNodeSettings,
    ) -> anyhow::Result<crate::sharing::ShareResult> {
        crate::sharing::SharingOperations::share_node(self, node_uid, settings).await
    }

    pub async fn unshare_node(
        &self,
        node_uid: NodeUid,
        settings: crate::sharing::UnshareNodeSettings,
    ) -> anyhow::Result<Option<crate::sharing::ShareResult>> {
        crate::sharing::SharingOperations::unshare_node(self, node_uid, settings).await
    }

    pub async fn set_editors_can_share(&self, node_uid: NodeUid, value: bool) -> anyhow::Result<()> {
        crate::sharing::SharingOperations::set_editors_can_share(self, node_uid, value).await
    }

    pub async fn create_public_link(
        &self,
        node_uid: NodeUid,
        settings: crate::sharing::ShareUrlSettings,
    ) -> anyhow::Result<crate::sharing::UrlAccess> {
        crate::sharing::SharingOperations::create_public_link(self, node_uid, settings).await
    }

    pub async fn get_public_link_info(
        &self,
        url: &str,
    ) -> anyhow::Result<crate::sharing::PublicLinkInfo> {
        crate::sharing::SharingOperations::get_public_link_info(self, url).await
    }

    pub async fn authenticate_public_link(
        &self,
        url: &str,
    ) -> anyhow::Result<crate::sharing::PublicLinkClient> {
        crate::sharing::SharingOperations::authenticate_public_link(self, url).await
    }

    pub async fn iterate_bookmarks(&self) -> anyhow::Result<Vec<crate::sharing::Bookmark>> {
        crate::sharing::SharingOperations::iterate_bookmarks(self).await
    }

    pub async fn create_bookmark(&self, url: &str) -> anyhow::Result<()> {
        crate::sharing::SharingOperations::create_bookmark(self, url).await
    }

    pub async fn remove_bookmark(&self, bookmark_or_url: &str) -> anyhow::Result<()> {
        crate::sharing::SharingOperations::remove_bookmark(self, bookmark_or_url).await
    }

    pub async fn subscribe_to_tree_events(
        &self,
        volume_id: VolumeId,
        callback: std::sync::Arc<dyn Fn(crate::events::DriveEvent) + Send + Sync>,
    ) -> anyhow::Result<crate::events::EventSubscription> {
        crate::events::subscribe_to_tree_events(self.clone(), volume_id, callback).await
    }

    pub async fn subscribe_to_drive_events(
        &self,
        callback: std::sync::Arc<dyn Fn(crate::events::DriveEvent) + Send + Sync>,
    ) -> anyhow::Result<crate::events::EventSubscription> {
        crate::events::subscribe_to_drive_events(self.clone(), callback).await
    }

    async fn get_file_uploader_from_draft_provider(
        &self,
        revision_draft_provider: Box<dyn RevisionDraftProvider>,
        size: i64,
        last_modification_time: Option<std::time::SystemTime>,
        additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
        media_info: Option<crate::api::attr::MediaExtendedAttributes>,
    ) -> anyhow::Result<FileUploader> {
        FileUploader::create(
            self,
            revision_draft_provider,
            size,
            last_modification_time,
            additional_metadata,
            media_info,
        )
        .await
    }
}

fn default_block_transfer_parallelism() -> usize {
    ProtonDriveClient::DEFAULT_DEGREE_OF_BLOCK_TRANSFER_PARALLELISM
}

fn generate_uid() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("drive-client-{}", nanos)
}
