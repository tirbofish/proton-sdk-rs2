use crate::cache::entity::{DefaultPhotosEntityCache, DriveEntityCache, PhotosEntityCache};
use crate::cache::secret::{DriveSecretCache, PhotosSecretCache};
use proton_sdk_rs2::cache::CacheRepository;
use std::sync::Arc;

pub trait DriveClientCache: Send + Sync {
    fn entities(&self) -> Arc<dyn DriveEntityCache>;
    fn secrets(&self) -> Arc<dyn DriveSecretCache>;
}

pub struct DefaultDriveClientCache {
    entities: Arc<dyn DriveEntityCache>,
    secrets: Arc<dyn DriveSecretCache>,
}

impl DefaultDriveClientCache {
    pub fn new(entities: Arc<dyn DriveEntityCache>, secrets: Arc<dyn DriveSecretCache>) -> Self {
        Self { entities, secrets }
    }
}

impl DriveClientCache for DefaultDriveClientCache {
    fn entities(&self) -> Arc<dyn DriveEntityCache> {
        self.entities.clone()
    }

    fn secrets(&self) -> Arc<dyn DriveSecretCache> {
        self.secrets.clone()
    }
}

pub trait PhotosClientCache: Send + Sync {
    fn entities(&self) -> Arc<dyn PhotosEntityCache>;
    fn secrets(&self) -> Arc<dyn DriveSecretCache>;
}

pub struct DefaultPhotosClientCache {
    entities: Arc<DefaultPhotosEntityCache>,
    secrets: Arc<dyn DriveSecretCache>,
}

impl DefaultPhotosClientCache {
    pub fn new(
        entity_repository: Arc<dyn CacheRepository>,
        secret_repository: Arc<dyn CacheRepository>,
    ) -> Self {
        Self {
            entities: Arc::new(DefaultPhotosEntityCache::new(entity_repository)),
            secrets: Arc::new(PhotosSecretCache::new(secret_repository)),
        }
    }
}

impl PhotosClientCache for DefaultPhotosClientCache {
    fn entities(&self) -> Arc<dyn PhotosEntityCache> {
        self.entities.clone()
    }
    fn secrets(&self) -> Arc<dyn DriveSecretCache> {
        self.secrets.clone()
    }
}
