use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose};
use tokio_util::sync::CancellationToken;

use crate::cache::CacheRepositoryTrait;

#[async_trait::async_trait]
pub trait SessionSecretCaching {
    async fn set_account_key_passphrase(
        &self,
        key_id: String,
        passphrase: &[u8],
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<()>;
    
    async fn try_get_account_key_passphrase(
        &self,
        key_id: String,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<Option<Vec<u8>>>;
}

pub struct SessionSecretCache {
    repository: Arc<dyn CacheRepositoryTrait>,
}

impl SessionSecretCache {
    pub(crate) fn new(repository: Arc<dyn CacheRepositoryTrait>) -> Self {
        Self {
            repository,
        }
    }

    fn get_account_passphrase_cache_key(key_id: &String) -> String {
        format!("account:passphrase:{}", key_id)
    }
}

#[async_trait::async_trait]
impl SessionSecretCaching for SessionSecretCache {
    async fn set_account_key_passphrase(
        &self,
        key_id: String,
        passphrase: &[u8],
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<()> {
        let cache_key = Self::get_account_passphrase_cache_key(&key_id);
        let serialized_value = general_purpose::STANDARD.encode(passphrase);
        
        Ok(
            self.repository
            .set(&cache_key, serialized_value, vec![], cancellation_token)
            .await?
        )
    }
    
    async fn try_get_account_key_passphrase(
        &self,
        key_id: String,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let cache_key = Self::get_account_passphrase_cache_key(&key_id);
        let serialized_value = self.repository
            .try_get(&cache_key, cancellation_token)
            .await?;
        
        match serialized_value {
            Some(value) => {
                let decoded = general_purpose::STANDARD.decode(value)?;
                Ok(Some(decoded))
            }
            None => Ok(None),
        }
    }
}