use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose};

use crate::cache::CacheRepository;

#[async_trait::async_trait]
pub trait SessionSecretCache: Send + Sync {
    async fn set_account_key_passphrase(
        &self,
        key_id: &String,
        passphrase: &[u8],
    ) -> anyhow::Result<()>;

    async fn try_get_account_key_passphrase(
        &self,
        key_id: &String,
    ) -> anyhow::Result<Option<Vec<u8>>>;
}

pub struct DefaultSecretCache {
    repository: Arc<dyn CacheRepository>,
}

impl DefaultSecretCache {
    pub(crate) fn new(repository: Arc<dyn CacheRepository>) -> Self {
        Self { repository }
    }

    fn get_account_passphrase_cache_key(key_id: &String) -> String {
        format!("account:passphrase:{}", key_id)
    }
}

#[async_trait::async_trait]
impl SessionSecretCache for DefaultSecretCache {
    async fn set_account_key_passphrase(
        &self,
        key_id: &String,
        passphrase: &[u8],
    ) -> anyhow::Result<()> {
        let serialized_value = general_purpose::STANDARD.encode(passphrase);

        let cache_key = Self::get_account_passphrase_cache_key(&key_id);
        self.repository
            .set(&cache_key, serialized_value.clone(), vec![])
            .await?;

        Ok(())
    }

    async fn try_get_account_key_passphrase(
        &self,
        key_id: &String,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let cache_key = Self::get_account_passphrase_cache_key(&key_id);
        log::debug!("Locating cache key {:?}", cache_key);
        let serialized_value = self.repository.try_get(&cache_key).await?;

        if serialized_value.is_none() {
            log::debug!("No serialized value");
        }
        log::debug!("Serialized value: {:?}", serialized_value);

        match serialized_value {
            Some(value) => {
                log::debug!("Serialized value: {:?}", value);
                let decoded = general_purpose::STANDARD.decode(value)?;
                Ok(Some(decoded))
            }
            None => Ok(None),
        }
    }
}
