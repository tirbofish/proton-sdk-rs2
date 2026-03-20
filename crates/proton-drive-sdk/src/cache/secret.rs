use crate::node::NodeUid;
use crate::node::file::{DegradedFileSecrets, FileSecrets};
use crate::node::folder::{DegradedFolderSecrets, FolderSecrets};
use crate::pgp::PgpPrivateKey;
use crate::share::ShareId;
use crate::utils::PotentialObject;
use async_trait::async_trait;
use proton_sdk_rs2::cache::CacheRepository;
use std::sync::Arc;

#[async_trait]
pub trait DriveSecretCache: Send + Sync {
    async fn set_share_key(
        &self,
        share_id: ShareId,
        share_key: PgpPrivateKey,
    ) -> anyhow::Result<()>;
    async fn try_get_share_key(&self, share_id: ShareId) -> anyhow::Result<Option<PgpPrivateKey>>;

    async fn set_folder_secrets(
        &self,
        node_id: NodeUid,
        secrets: PotentialObject<FolderSecrets, DegradedFolderSecrets>,
    ) -> anyhow::Result<()>;
    async fn try_get_folder_secrets(
        &self,
        node_id: NodeUid,
    ) -> anyhow::Result<Option<PotentialObject<FolderSecrets, DegradedFolderSecrets>>>;

    async fn set_file_secrets(
        &self,
        node_id: NodeUid,
        secrets: PotentialObject<FileSecrets, DegradedFileSecrets>,
    ) -> anyhow::Result<()>;
    async fn try_get_file_secrets(
        &self,
        node_id: NodeUid,
    ) -> anyhow::Result<Option<PotentialObject<FileSecrets, DegradedFileSecrets>>>;

    async fn clear(&self) -> anyhow::Result<()>;
}

pub struct PhotosSecretCache {
    repository: Arc<dyn CacheRepository>,
}

impl PhotosSecretCache {
    pub fn new(repository: Arc<dyn CacheRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait]
impl DriveSecretCache for PhotosSecretCache {
    async fn set_share_key(
        &self,
        share_id: ShareId,
        share_key: PgpPrivateKey,
    ) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&share_key)?;
        self.repository
            .set(&share_key_cache_key(&share_id), serialized, vec![])
            .await
    }

    async fn try_get_share_key(&self, _share_id: ShareId) -> anyhow::Result<Option<PgpPrivateKey>> {
        anyhow::bail!("try_get_share_key is not supported on PhotosSecretCache")
    }

    async fn set_folder_secrets(
        &self,
        node_id: NodeUid,
        secrets: PotentialObject<FolderSecrets, DegradedFolderSecrets>,
    ) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&secrets)?;
        self.repository
            .set(&folder_secrets_cache_key(&node_id), serialized, vec![])
            .await
    }

    async fn try_get_folder_secrets(
        &self,
        node_id: NodeUid,
    ) -> anyhow::Result<Option<PotentialObject<FolderSecrets, DegradedFolderSecrets>>> {
        let value = self
            .repository
            .try_get(&folder_secrets_cache_key(&node_id))
            .await?;
        Ok(match value {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    async fn set_file_secrets(
        &self,
        node_id: NodeUid,
        secrets: PotentialObject<FileSecrets, DegradedFileSecrets>,
    ) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&secrets)?;
        self.repository
            .set(&file_secrets_cache_key(&node_id), serialized, vec![])
            .await
    }

    async fn try_get_file_secrets(
        &self,
        _node_id: NodeUid,
    ) -> anyhow::Result<Option<PotentialObject<FileSecrets, DegradedFileSecrets>>> {
        anyhow::bail!("try_get_file_secrets is not supported on PhotosSecretCache")
    }

    async fn clear(&self) -> anyhow::Result<()> {
        self.repository.clear().await
    }
}

fn share_key_cache_key(share_id: &ShareId) -> String {
    format!("share_key_{}", share_id.raw())
}

fn folder_secrets_cache_key(node_id: &NodeUid) -> String {
    format!("folder_secrets_{}", node_id)
}

fn file_secrets_cache_key(node_id: &NodeUid) -> String {
    format!("file_secrets_{}", node_id)
}

pub struct DefaultDriveSecretCache {
    repository: Arc<dyn CacheRepository>,
}

impl DefaultDriveSecretCache {
    pub fn new(repository: Arc<dyn CacheRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait]
impl DriveSecretCache for DefaultDriveSecretCache {
    async fn set_share_key(
        &self,
        share_id: ShareId,
        share_key: PgpPrivateKey,
    ) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&share_key)?;
        self.repository
            .set(&share_key_cache_key(&share_id), serialized, vec![])
            .await
    }

    async fn try_get_share_key(&self, share_id: ShareId) -> anyhow::Result<Option<PgpPrivateKey>> {
        let value = self
            .repository
            .try_get(&share_key_cache_key(&share_id))
            .await?;
        Ok(match value {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    async fn set_folder_secrets(
        &self,
        node_id: NodeUid,
        secrets: PotentialObject<FolderSecrets, DegradedFolderSecrets>,
    ) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&secrets)?;
        self.repository
            .set(&folder_secrets_cache_key(&node_id), serialized, vec![])
            .await
    }

    async fn try_get_folder_secrets(
        &self,
        node_id: NodeUid,
    ) -> anyhow::Result<Option<PotentialObject<FolderSecrets, DegradedFolderSecrets>>> {
        let value = self
            .repository
            .try_get(&folder_secrets_cache_key(&node_id))
            .await?;
        Ok(match value {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    async fn set_file_secrets(
        &self,
        node_id: NodeUid,
        secrets: PotentialObject<FileSecrets, DegradedFileSecrets>,
    ) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&secrets)?;
        self.repository
            .set(&file_secrets_cache_key(&node_id), serialized, vec![])
            .await
    }

    async fn try_get_file_secrets(
        &self,
        node_id: NodeUid,
    ) -> anyhow::Result<Option<PotentialObject<FileSecrets, DegradedFileSecrets>>> {
        let value = self
            .repository
            .try_get(&file_secrets_cache_key(&node_id))
            .await?;
        Ok(match value {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    async fn clear(&self) -> anyhow::Result<()> {
        self.repository.clear().await
    }
}
