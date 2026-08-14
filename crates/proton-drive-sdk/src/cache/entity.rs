use crate::cache::CachedNodeInfo;
use crate::node::{DegradedNode, Node, NodeUid};
use crate::share::{Share, ShareId};
use crate::utils::PotentialObject;
use crate::volume::VolumeId;
use async_trait::async_trait;
use proton_sdk_rs2::cache::CacheRepository;
use std::sync::Arc;

const CLIENT_UID_KEY: &str = "client:id";
const MAIN_VOLUME_ID_KEY: &str = "volume:main:id";
const MY_FILES_SHARE_ID_KEY: &str = "share:my-files:id";
const PHOTOS_VOLUME_ID_KEY: &str = "volume:photos:id";
const PHOTOS_SHARE_ID_KEY: &str = "share:photos:id";

fn photos_volume_id_key() -> &'static str {
    PHOTOS_VOLUME_ID_KEY
}

fn photos_share_id_key() -> &'static str {
    PHOTOS_SHARE_ID_KEY
}

#[async_trait]
pub trait EntityCache: Send + Sync {
    async fn set_node(
        &self,
        node_id: NodeUid,
        node_provision_result: PotentialObject<Node, DegradedNode>,
        membership_share_id: Option<ShareId>,
        name_hash_digest: Vec<u8>,
    ) -> anyhow::Result<()>;

    async fn try_get_node(&self, node_id: NodeUid) -> anyhow::Result<Option<CachedNodeInfo>>;
}

#[async_trait]
pub trait DriveEntityCache: EntityCache + Send + Sync {
    async fn set_client_uid(&self, client_uid: String) -> anyhow::Result<()>;
    async fn try_get_client_uid(&self) -> anyhow::Result<Option<String>>;
    async fn set_main_volume_id(&self, volume_id: VolumeId) -> anyhow::Result<()>;
    async fn try_get_main_volume_id(&self) -> anyhow::Result<Option<VolumeId>>;
    async fn set_my_files_share_id(&self, share_id: ShareId) -> anyhow::Result<()>;
    async fn try_get_my_files_share_id(&self) -> anyhow::Result<Option<ShareId>>;
    async fn set_share(&self, share: Share) -> anyhow::Result<()>;
    async fn try_get_share(&self, share_id: ShareId) -> anyhow::Result<Option<Share>>;
    async fn remove_node(&self, node_uid: NodeUid) -> anyhow::Result<()>;
}

fn share_cache_key(share_id: &ShareId) -> String {
    format!("share:{}", share_id.raw())
}

fn node_cache_key(node_id: &NodeUid) -> String {
    format!("node:{}", node_id.raw())
}

pub struct DefaultDriveEntityCache {
    repository: Arc<dyn CacheRepository>,
}

impl DefaultDriveEntityCache {
    pub fn new(repository: Arc<dyn CacheRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait]
impl EntityCache for DefaultDriveEntityCache {
    async fn set_node(
        &self,
        node_id: NodeUid,
        node_provision_result: PotentialObject<Node, DegradedNode>,
        membership_share_id: Option<ShareId>,
        name_hash_digest: Vec<u8>,
    ) -> anyhow::Result<()> {
        let info = CachedNodeInfo {
            node_provision_result,
            membership_share_id,
            name_hash_digest,
        };
        let serialized = serde_json::to_string(&info)?;
        self.repository
            .set(&node_cache_key(&node_id), serialized, vec![])
            .await
    }

    async fn try_get_node(&self, node_id: NodeUid) -> anyhow::Result<Option<CachedNodeInfo>> {
        let value = self.repository.try_get(&node_cache_key(&node_id)).await?;
        Ok(match value {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }
}

#[async_trait]
impl DriveEntityCache for DefaultDriveEntityCache {
    async fn set_client_uid(&self, client_uid: String) -> anyhow::Result<()> {
        self.repository
            .set(CLIENT_UID_KEY, client_uid, vec![])
            .await
    }

    async fn try_get_client_uid(&self) -> anyhow::Result<Option<String>> {
        self.repository.try_get(CLIENT_UID_KEY).await
    }

    async fn set_main_volume_id(&self, volume_id: VolumeId) -> anyhow::Result<()> {
        self.repository
            .set(MAIN_VOLUME_ID_KEY, volume_id.raw().to_string(), vec![])
            .await
    }

    async fn try_get_main_volume_id(&self) -> anyhow::Result<Option<VolumeId>> {
        let value = self.repository.try_get(MAIN_VOLUME_ID_KEY).await?;
        Ok(value.map(VolumeId::new))
    }

    async fn set_my_files_share_id(&self, share_id: ShareId) -> anyhow::Result<()> {
        self.repository
            .set(MY_FILES_SHARE_ID_KEY, share_id.raw().to_string(), vec![])
            .await
    }

    async fn try_get_my_files_share_id(&self) -> anyhow::Result<Option<ShareId>> {
        let value = self.repository.try_get(MY_FILES_SHARE_ID_KEY).await?;
        Ok(value.map(ShareId::new))
    }

    async fn set_share(&self, share: Share) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&share)?;
        self.repository
            .set(&share_cache_key(&share.id), serialized, vec![])
            .await
    }

    async fn try_get_share(&self, share_id: ShareId) -> anyhow::Result<Option<Share>> {
        let value = self.repository.try_get(&share_cache_key(&share_id)).await?;
        Ok(match value {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    async fn remove_node(&self, node_uid: NodeUid) -> anyhow::Result<()> {
        self.repository.remove(&node_cache_key(&node_uid)).await
    }
}

#[async_trait]
pub trait PhotosEntityCache: Send + Sync {
    async fn set_photos_volume_id(&self, volume_id: VolumeId) -> anyhow::Result<()>;
    async fn try_get_photos_volume_id(&self) -> anyhow::Result<Option<VolumeId>>;
    async fn set_photos_share_id(&self, share_id: ShareId) -> anyhow::Result<()>;
    async fn try_get_photos_share_id(&self) -> anyhow::Result<Option<ShareId>>;
}

pub struct DefaultPhotosEntityCache {
    repository: Arc<dyn CacheRepository>,
}

impl DefaultPhotosEntityCache {
    pub fn new(repository: Arc<dyn CacheRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait]
impl PhotosEntityCache for DefaultPhotosEntityCache {
    async fn set_photos_volume_id(&self, volume_id: VolumeId) -> anyhow::Result<()> {
        self.repository
            .set(photos_volume_id_key(), volume_id.raw().to_string(), vec![])
            .await
    }

    async fn try_get_photos_volume_id(&self) -> anyhow::Result<Option<VolumeId>> {
        Ok(self
            .repository
            .try_get(photos_volume_id_key())
            .await?
            .map(VolumeId::new))
    }

    async fn set_photos_share_id(&self, share_id: ShareId) -> anyhow::Result<()> {
        self.repository
            .set(photos_share_id_key(), share_id.raw().to_string(), vec![])
            .await
    }

    async fn try_get_photos_share_id(&self) -> anyhow::Result<Option<ShareId>> {
        Ok(self
            .repository
            .try_get(photos_share_id_key())
            .await?
            .map(ShareId::new))
    }
}
