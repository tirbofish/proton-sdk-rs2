use std::collections::HashMap;
use std::sync::Arc;

use crate::account::{AccountClient, AccountClientAdapter};
use crate::api::DriveApiClients;
use crate::api::{DefaultDriveApiClientsFactory, DriveApiClientsFactory};
use crate::block::download::BlockDownloader;
use crate::block::upload::BlockUploader;
use crate::block::verify::{BlockVerifierFactory, DefaultBlockVerifierFactory};
use crate::cache::client::{DefaultDriveClientCache, DriveClientCache};
use crate::cache::entity::{DefaultDriveEntityCache, DriveEntityCache};
use crate::cache::secret::{DefaultDriveSecretCache, DriveSecretCache};
use crate::meta::AdditionalMetadataProperty;
use crate::node::draft::{NewFileDraftProvider, NewRevisionDraftProvider, RevisionDraftProvider};
use crate::node::file::FileOperations;
use crate::node::file::FileThumbnail;
use crate::node::file::download::FileDownloader;
use crate::node::file::upload::FileUploader;
use crate::node::folder::{FolderNode, FolderOperations};
use crate::node::operations::NodeOperations;
use crate::node::revision::{REVISION_WRITER_DEFAULT_BLOCK_SIZE, RevisionUid};
use crate::node::thumbnail::ThumbnailType;
use crate::node::{DegradedNode, Node, NodeUid};
use crate::volume::VolumeId;
use crate::links::LinkId;
use crate::utils::PotentialObject;
use crate::utils::semaphore::FifoFlexibleSemaphore;
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
    pub api_call_timeout: Option<u32>,
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
}

// initialisers
impl ProtonDriveClient {
    const MIN_DEGREE_OF_BLOCK_TRANSFER_PARALLELISM: usize = 2;
    const MAX_DEGREE_OF_BLOCK_TRANSFER_PARALLELISM: usize = 6;

    pub fn new(session: &ProtonAPISession, uid: Option<String>) -> anyhow::Result<Self> {
        Self::from_session_with_drive_api_clients_factory(
            session,
            Arc::new(DefaultDriveApiClientsFactory),
            uid,
        )
    }

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
        let thumbnail_block_downloader = BlockDownloader::new(8);

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
        }
    }
}

// getters
impl ProtonDriveClient {
    pub fn uid(&self) -> &str {
        &self.uid
    }

    pub fn target_block_size(&self) -> usize {
        self.target_block_size
    }

    pub fn set_target_block_size(&mut self, value: usize) {
        self.target_block_size = value;
    }

    pub(crate) fn account(&self) -> &Arc<dyn AccountClient> {
        &self.account
    }

    // this should always be pub(crate), not pub. reduce the visibility.
    pub fn api(&self) -> &Arc<dyn DriveApiClients> {
        &self.api
    }

    pub async fn list_children(
        &self,
        volume_id: VolumeId,
        parent_link_id: Option<LinkId>,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        let parent_uid = parent_link_id.map(|id| NodeUid::new(volume_id.clone(), id));
        let mut stream = match parent_uid {
            Some(uid) => self.enumerate_folder_children(uid).await?,
            None => {
                let root = self.get_my_files_folder().await?;
                self.enumerate_folder_children(root.base.uid).await?
            }
        };
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

    pub fn telemetry(&self) -> &Arc<dyn Telemetry> {
        &self.telemetry
    }

    pub fn feature_flag_provider(&self) -> &Arc<dyn FeatureFlagProvider> {
        &self.feature_flag_provider
    }

    pub fn revision_creation_semaphore(&self) -> &FifoFlexibleSemaphore {
        &self.revision_creation_semaphore
    }

    pub fn block_listing_semaphore(&self) -> &FifoFlexibleSemaphore {
        &self.block_listing_semaphore
    }

    pub fn block_uploader(&self) -> &BlockUploader {
        &self.block_uploader
    }

    pub fn block_downloader(&self) -> &BlockDownloader {
        &self.block_downloader
    }

    pub fn thumbnail_block_downloader(&self) -> &BlockDownloader {
        &self.thumbnail_block_downloader
    }
}

// the meat
impl ProtonDriveClient {
    pub async fn get_my_files_folder(&self) -> anyhow::Result<FolderNode> {
        NodeOperations::get_my_files_folder(self).await
    }

    pub async fn get_node(
        &self,
        node_uid: NodeUid,
    ) -> anyhow::Result<PotentialObject<Node, DegradedNode>> {
        NodeOperations::get_node(self, node_uid).await
    }

    pub async fn enumerate_nodes(
        &self,
        node_uids: Vec<NodeUid>,
    ) -> anyhow::Result<
        impl futures::Stream<Item = anyhow::Result<PotentialObject<Node, DegradedNode>>> + '_,
    > {
        let results = NodeOperations::enumerate_nodes(self, node_uids).await?;
        Ok(futures::stream::iter(results.into_iter().map(Ok)))
    }

    pub async fn create_folder(
        &self,
        parent_id: NodeUid,
        name: String,
        last_modification_time: Option<std::time::SystemTime>,
    ) -> anyhow::Result<FolderNode> {
        FolderOperations::create(self, parent_id, name, last_modification_time).await
    }

    pub async fn enumerate_folder_children(
        &self,
        folder_id: NodeUid,
    ) -> anyhow::Result<
        impl futures::Stream<Item = anyhow::Result<PotentialObject<Node, DegradedNode>>> + '_,
    > {
        let results = FolderOperations::enumerate_children(self, folder_id).await?;
        Ok(futures::stream::iter(results.into_iter().map(Ok)))
    }

    pub async fn enumerate_thumbnails(
        &self,
        file_uids: Vec<NodeUid>,
        thumbnail_type: ThumbnailType,
    ) -> anyhow::Result<impl futures::Stream<Item = anyhow::Result<FileThumbnail>> + '_> {
        let results = FileOperations::enumerate_thumbnails(self, file_uids, thumbnail_type).await?;
        Ok(futures::stream::iter(results.into_iter().map(Ok)))
    }

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

    pub async fn get_file_downloader(
        &self,
        revision_uid: RevisionUid,
    ) -> anyhow::Result<FileDownloader> {
        FileDownloader::create(self, revision_uid).await
    }

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

    pub async fn upload_file(
        &self,
        path: &std::path::Path,
        parent_folder_uid: NodeUid,
        override_existing: bool,
        on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> anyhow::Result<()> {
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
        let (thumbnails, media_info) = if media_type.starts_with("image/") {
            crate::utils::thumbnail::ThumbnailGenerator::generate_thumbnails(&file_data)
        } else {
            (Vec::new(), None)
        };

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
            .upload_from_stream(Box::new(std::io::Cursor::new(file_data)), thumbnails, on_progress)
            .await
    }

    pub async fn get_available_name(
        &self,
        parent_uid: NodeUid,
        name: String,
    ) -> anyhow::Result<String> {
        NodeOperations::get_available_name(self, parent_uid, name).await
    }

    pub async fn move_nodes(
        &self,
        uids: Vec<NodeUid>,
        new_parent_folder_uid: NodeUid,
    ) -> anyhow::Result<()> {
        NodeOperations::move_multiple(self, uids, new_parent_folder_uid).await
    }

    pub async fn rename_node(
        &self,
        uid: NodeUid,
        new_name: String,
        new_media_type: Option<String>,
    ) -> anyhow::Result<()> {
        NodeOperations::rename(self, uid, new_name, new_media_type).await
    }

    pub async fn trash_nodes(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        NodeOperations::trash(self, uids).await
    }

    pub async fn delete_nodes(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        NodeOperations::delete(self, uids).await
    }

    pub async fn delete_nodes_from_trash(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        NodeOperations::delete_from_trash(self, uids).await
    }

    pub async fn restore_nodes(
        &self,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        NodeOperations::restore(self, uids).await
    }

    pub async fn enumerate_trash(&self) -> anyhow::Result<Vec<Result<Node, DegradedNode>>> {
        VolumeOperations::enumerate_trash(self).await
    }

    pub async fn empty_trash(&self) -> anyhow::Result<()> {
        VolumeOperations::empty_trash(self).await
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
    (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(ProtonDriveClient::MIN_DEGREE_OF_BLOCK_TRANSFER_PARALLELISM)
        / 2)
    .clamp(
        ProtonDriveClient::MIN_DEGREE_OF_BLOCK_TRANSFER_PARALLELISM,
        ProtonDriveClient::MAX_DEGREE_OF_BLOCK_TRANSFER_PARALLELISM,
    )
}

fn generate_uid() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("drive-client-{}", nanos)
}
