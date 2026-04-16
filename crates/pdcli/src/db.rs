use std::path::PathBuf;
use std::sync::Arc;

use platform_dirs::AppDirs;
use proton_drive_sdk::cache::keyring::KeyringSecretCache;
use proton_drive_sdk::cache::secret::DriveSecretCache;
use proton_drive_sdk::cache::sqlite::SqliteCacheRepository;
use proton_sdk_rs2::cache::CacheRepository;

const APP_NAME: &str = "pdcli";
const ENTITY_DB: &str = "entities.db";
const KEYRING_SERVICE: &str = "pdcli-secrets";

fn data_dir() -> PathBuf {
    AppDirs::new(Some(APP_NAME), false)
        .expect("failed to resolve platform data directory")
        .data_dir
}

/// Central indexed cache that mediates between online APIs and the FUSE layer.
///
/// The FUSE filesystem reads exclusively from this cache. Online operations
/// (browsing folders, fetching metadata, receiving events) write into it.
/// This ensures the network never touches FUSE directly.
pub struct SQLIndexedCache {
    /// Key-value entity store backed by SQLite on disk.
    /// Stores shares, nodes, volumes and other Drive metadata.
    entity_repository: Arc<SqliteCacheRepository>,
    /// Secrets stored in the OS keyring (Keychain / Secret Service / Credential Manager).
    secret_cache: Arc<KeyringSecretCache>,
}

impl SQLIndexedCache {
    /// Opens (or creates) the indexed cache.
    ///
    /// - Entities are persisted to `<data_dir>/entities.db`.
    /// - Secrets are stored in the system keyring under service `pdcli-secrets`.
    pub fn open() -> anyhow::Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        let db_path = dir.join(ENTITY_DB);

        tracing::info!(path = %db_path.display(), "opening entity cache");

        let entity_repository = Arc::new(SqliteCacheRepository::open_file(&db_path, None)?);
        let secret_cache = Arc::new(KeyringSecretCache::new(KEYRING_SERVICE));

        Ok(Self {
            entity_repository,
            secret_cache,
        })
    }

    /// Returns the entity cache repository for use with `ProtonClientOptions`
    /// and `DefaultDriveEntityCache`.
    pub fn entity_repository(&self) -> Arc<dyn CacheRepository> {
        self.entity_repository.clone()
    }

    /// Returns the secret cache as an `Arc<dyn CacheRepository>` for use with
    /// `ProtonClientOptions::secret_cache_repository`.
    ///
    /// Note: this is the *raw* repository interface. For the higher-level
    /// `DriveSecretCache` trait used by `ProtonDriveClient`, use
    /// [`Self::secret_cache`].
    pub fn secret_repository(&self) -> Arc<SqliteCacheRepository> {
        self.entity_repository.clone()
    }

    /// Returns the keyring-backed secret cache for use with
    /// `DefaultDriveClientCache` / `ProtonDriveClient`.
    pub fn secret_cache(&self) -> Arc<dyn DriveSecretCache> {
        self.secret_cache.clone()
    }
}