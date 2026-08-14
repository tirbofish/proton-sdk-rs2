//! System keyring storage of secrets
//!
//! This module provides a [`DriveSecretCache`] implementation that stores secrets
//! in the operating system's secure keyring (Keychain on macOS, Secret Service on
//! Linux, Credential Manager on Windows).
//!
//! Can be enabled with the feature `cache-keyring`.
//!
//! # Example
//!
//! ```no_run
//! use proton_drive_sdk::cache::keyring::KeyringSecretCache;
//!
//! let cache = KeyringSecretCache::new("my-app");
//! // Use with ProtonDriveClient via ProtonClientOptions
//! ```
//!
//! # Limitations
//!
//! - The `clear()` method only clears tracked keys from the current session.
//! - Keys stored in previous sessions cannot be enumerated from the keyring.
//!   You are able to resume your previous session and use that keyring, just not access previous sessions.
//! - Keyring operations are blocking and are offloaded to a thread pool.

use crate::cache::secret::DriveSecretCache;
use crate::node::NodeUid;
use crate::node::file::{DegradedFileSecrets, FileSecrets};
use crate::node::folder::{DegradedFolderSecrets, FolderSecrets};
use crate::pgp::PgpPrivateKey;
use crate::share::ShareId;
use crate::utils::PotentialObject;
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;

/// A [`DriveSecretCache`] implementation that stores secrets in the system keyring.
///
/// This provides secure storage of cryptographic keys and secrets using the
/// operating system's native secret storage mechanism.
pub struct KeyringSecretCache {
    /// The service name used for keyring entries (e.g., "proton-drive-myapp").
    service_name: String,
    /// Tracks keys stored in this session for the `clear()` implementation.
    tracked_keys: Arc<Mutex<HashSet<String>>>,
}

impl KeyringSecretCache {
    /// Creates a new `KeyringSecretCache` with the given service name.
    ///
    /// The service name is used to namespace keyring entries. It should be unique
    /// to your application to avoid conflicts with other applications.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use proton_drive_sdk::cache::keyring::KeyringSecretCache;
    ///
    /// let cache = KeyringSecretCache::new("my-proton-app");
    /// ```
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            tracked_keys: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Sets a value in the keyring, tracking the key for later cleanup.
    async fn set_value(&self, key: String, value: String) -> anyhow::Result<()> {
        let service = self.service_name.clone();
        let tracked = self.tracked_keys.clone();

        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(&service, &key)
                .map_err(|e| anyhow::anyhow!("Failed to create keyring entry: {}", e))?;
            entry
                .set_password(&value)
                .map_err(|e| anyhow::anyhow!("Failed to store in keyring: {}", e))?;
            tracked.lock().insert(key);
            Ok(())
        })
        .await?
    }

    /// Gets a value from the keyring.
    async fn get_value(&self, key: String) -> anyhow::Result<Option<String>> {
        let service = self.service_name.clone();

        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(&service, &key)
                .map_err(|e| anyhow::anyhow!("Failed to create keyring entry: {}", e))?;
            match entry.get_password() {
                Ok(value) => Ok(Some(value)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(anyhow::anyhow!("Failed to retrieve from keyring: {}", e)),
            }
        })
        .await?
    }

    /// Removes a value from the keyring.
    async fn remove_value(&self, key: String) -> anyhow::Result<()> {
        let service = self.service_name.clone();
        let tracked = self.tracked_keys.clone();

        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(&service, &key)
                .map_err(|e| anyhow::anyhow!("Failed to create keyring entry: {}", e))?;
            match entry.delete_credential() {
                Ok(()) => {
                    tracked.lock().remove(&key);
                    Ok(())
                }
                Err(keyring::Error::NoEntry) => Ok(()), // Already deleted
                Err(e) => Err(anyhow::anyhow!("Failed to delete from keyring: {}", e)),
            }
        })
        .await?
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

#[async_trait]
impl DriveSecretCache for KeyringSecretCache {
    async fn set_share_key(
        &self,
        share_id: ShareId,
        share_key: PgpPrivateKey,
    ) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&share_key)?;
        self.set_value(share_key_cache_key(&share_id), serialized)
            .await
    }

    async fn try_get_share_key(&self, share_id: ShareId) -> anyhow::Result<Option<PgpPrivateKey>> {
        match self.get_value(share_key_cache_key(&share_id)).await? {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    async fn set_folder_secrets(
        &self,
        node_id: NodeUid,
        secrets: PotentialObject<FolderSecrets, DegradedFolderSecrets>,
    ) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&secrets)?;
        self.set_value(folder_secrets_cache_key(&node_id), serialized)
            .await
    }

    async fn try_get_folder_secrets(
        &self,
        node_id: NodeUid,
    ) -> anyhow::Result<Option<PotentialObject<FolderSecrets, DegradedFolderSecrets>>> {
        match self.get_value(folder_secrets_cache_key(&node_id)).await? {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    async fn set_file_secrets(
        &self,
        node_id: NodeUid,
        secrets: PotentialObject<FileSecrets, DegradedFileSecrets>,
    ) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&secrets)?;
        self.set_value(file_secrets_cache_key(&node_id), serialized)
            .await
    }

    async fn try_get_file_secrets(
        &self,
        node_id: NodeUid,
    ) -> anyhow::Result<Option<PotentialObject<FileSecrets, DegradedFileSecrets>>> {
        match self.get_value(file_secrets_cache_key(&node_id)).await? {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    async fn clear(&self) -> anyhow::Result<()> {
        // Get all tracked keys and clear them
        let keys: Vec<String> = {
            let mut tracked = self.tracked_keys.lock();
            let keys: Vec<_> = tracked.drain().collect();
            keys
        };

        for key in keys {
            // Ignore errors during clear, key might already be deleted
            let _ = self.remove_value(key).await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// persistence test
    #[tokio::test]
    async fn test_keyring_persistence() {
        let cache = KeyringSecretCache::new("proton-drive-sdk-test");
        let test_key = "persistent_test_key".to_string();
        let test_value = "persistent_test_value".to_string();

        // first get might return none on first time or some on sequent runs.
        let initial: anyhow::Result<Option<String>> = cache.get_value(test_key.clone()).await;
        let initial = initial.expect("get_value should not error");
        println!("Initial get: {:?}", initial);

        // set
        let set_result: anyhow::Result<()> =
            cache.set_value(test_key.clone(), test_value.clone()).await;
        set_result.expect("Failed to set value in keyring");
        println!("Set value: {}", test_value);

        // get again (should succeed)
        let final_get: anyhow::Result<Option<String>> = cache.get_value(test_key).await;
        let final_get = final_get.expect("get_value should not error");
        println!("Final get: {:?}", final_get);

        assert_eq!(
            final_get,
            Some(test_value),
            "Value should be retrievable after set"
        );
    }
}
