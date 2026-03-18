use std::sync::Arc;
use std::time::{Duration, Instant};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose};
use dashmap::DashMap;
use futures::StreamExt;
use proton_rpgp::{DataEncoding, PrivateKey, PublicKey};
use serde::{Deserialize, Serialize};

use crate::cache::CacheRepository;
use crate::protobuf::{Address, AddressKey};
use crate::secret::SessionSecretCache;

// trait ------------------------------------------
pub trait AccountClientCache: Send + Sync {
    fn entities(&self) -> Arc<dyn AccountEntityCache>;
    fn secrets(&self) -> Arc<dyn AccountSecretCache>;
    fn session_secrets(&self) -> Arc<dyn SessionSecretCache>;
    fn public_keys(&self) -> Arc<dyn PublicKeyCache>;
}

// impl -----------------------

pub struct DefaultAccountClientCache {
    entity_cache_repository: Arc<dyn AccountEntityCache>,
    secret_cache_repository: Arc<dyn AccountSecretCache>,
    session_secret_cache_repository: Arc<dyn SessionSecretCache>,
    public_key_cache_repository: Arc<dyn PublicKeyCache>,
}

impl DefaultAccountClientCache {
    pub fn new(
        entity_cache_repository: Arc<dyn CacheRepository>,
        secret_cache_repository: Arc<dyn CacheRepository>,
        session_secret_cache_repository: Arc<dyn SessionSecretCache>,
    ) -> Self {
        Self {
            entity_cache_repository: Arc::new(DefaultAccountEntityCache::new(entity_cache_repository)),
            secret_cache_repository: Arc::new(DefaultAccountSecretCache::new(secret_cache_repository)),
            session_secret_cache_repository,
            public_key_cache_repository: Arc::new(DefaultPublicKeyCache::new()),
        }
    }
}

impl AccountClientCache for DefaultAccountClientCache {
    fn entities(&self) -> Arc<dyn AccountEntityCache> {
        self.entity_cache_repository.clone()
    }

    fn secrets(&self) -> Arc<dyn AccountSecretCache> {
        self.secret_cache_repository.clone()
    }

    fn session_secrets(&self) -> Arc<dyn SessionSecretCache> {
        self.session_secret_cache_repository.clone()
    }

    fn public_keys(&self) -> Arc<dyn PublicKeyCache> {
        self.public_key_cache_repository.clone()
    }
}

// trait ------------------------------------------

#[async_trait]
pub trait AccountEntityCache: Send + Sync {
    async fn set_address(
        &self,
        address: &Address,
    ) -> anyhow::Result<()>;

    async fn try_get_address(
        &self,
        address_id: &str,
    ) -> anyhow::Result<Option<Address>>;

    async fn set_current_user_addresses(
        &self,
        addresses: &[Address],
    ) -> anyhow::Result<()>;

    async fn try_get_current_user_addresses(
        &self,
    ) -> anyhow::Result<Option<Vec<Address>>>;
}

pub struct DefaultAccountEntityCache {
    repository: Arc<dyn CacheRepository>
}

impl DefaultAccountEntityCache {
    const CURRENT_USER_ADDRESS_TAG: &'static str = "user:current:address";

    pub fn new(repository: Arc<dyn CacheRepository>) -> Self {
        Self {
            repository,
        }
    }

    fn get_address_cache_key(address_id: &str) -> String {
        format!("address:{}", address_id)
    }
}

#[async_trait]
impl AccountEntityCache for DefaultAccountEntityCache {
    async fn set_address(
        &self,
        address: &Address,
    ) -> anyhow::Result<()> {
        let key = Self::get_address_cache_key(&address.address_id);
        let value = serde_json::to_string(&AddressCacheValue::from_address(address))?;
        self.repository
            .set(&key, value, vec![], )
            .await
    }

    async fn try_get_address(
        &self,
        address_id: &str,
    ) -> anyhow::Result<Option<Address>> {
        let key = Self::get_address_cache_key(address_id);
        let cached = self.repository.try_get(&key, ).await?;
        let result = match cached {
            Some(raw) => {
                let value: AddressCacheValue = serde_json::from_str(&raw)?;
                Some(value.into_address())
            }
            None => None,
        };
        Ok(result)
    }

    async fn set_current_user_addresses(
        &self,
        addresses: &[Address],
    ) -> anyhow::Result<()> {
        self.repository
            .remove_by_tag(Self::CURRENT_USER_ADDRESS_TAG, )
            .await?;

        for address in addresses {
            let key = Self::get_address_cache_key(&address.address_id);
            let value = serde_json::to_string(&AddressCacheValue::from_address(address))?;
            self.repository
                .set(
                    &key,
                    value,
                    vec![Self::CURRENT_USER_ADDRESS_TAG.to_string()],
                    
                )
                .await?;
        }

        Ok(())
    }

    async fn try_get_current_user_addresses(
        &self,
    ) -> anyhow::Result<Option<Vec<Address>>> {
        let mut stream = self.repository.get_by_tags(
            vec![Self::CURRENT_USER_ADDRESS_TAG.to_string()],
            
        );

        let mut addresses = Vec::new();
        while let Some(item) = stream.next().await {
            let (_, raw) = item?;
            let value: AddressCacheValue = serde_json::from_str(&raw)?;
            addresses.push(value.into_address());
        }

        if addresses.is_empty() {
            Ok(None)
        } else {
            addresses.sort_by_key(|a| a.order);
            Ok(Some(addresses))
        }
    }
}


// impl -----------------------

#[async_trait]
pub trait AccountSecretCache: Send + Sync {
    async fn set_user_keys(
        &self,
        unlocked_keys: &[PrivateKey],
    ) -> anyhow::Result<()>;

    async fn try_get_user_keys(
        &self,
    ) -> anyhow::Result<Option<Vec<PrivateKey>>>;

    async fn set_address_keys(
        &self,
        address_id: &str,
        unlocked_keys: &[PrivateKey],
    ) -> anyhow::Result<()>;

    async fn try_get_address_keys(
        &self,
        address_id: &str,
    ) -> anyhow::Result<Option<Vec<PrivateKey>>>;
}

// impl -----------------------

pub struct DefaultAccountSecretCache {
    repository: Arc<dyn CacheRepository>
}

impl DefaultAccountSecretCache {
    const USER_KEYS_CACHE_KEY: &'static str = "user:current:keys";

    pub fn new(repository: Arc<dyn CacheRepository>) -> Self {
        Self {
            repository,
        }
    }

    fn get_address_keys_cache_key(address_id: &str) -> String {
        format!("address:{}:keys", address_id)
    }

    fn serialize_private_keys(keys: &[PrivateKey]) -> anyhow::Result<String> {
        let mut armored = Vec::with_capacity(keys.len());
        for key in keys {
            let bytes = key.export_unlocked(DataEncoding::Armored)?;
            armored.push(general_purpose::STANDARD.encode(bytes));
        }
        Ok(serde_json::to_string(&armored)?)
    }

    fn deserialize_private_keys(raw: &str) -> anyhow::Result<Vec<PrivateKey>> {
        let serialized: Vec<String> = serde_json::from_str(raw)?;
        let mut keys = Vec::with_capacity(serialized.len());
        for item in serialized {
            let bytes = general_purpose::STANDARD.decode(item)?;
            keys.push(PrivateKey::import_unlocked(&bytes, DataEncoding::Armored)?);
        }
        Ok(keys)
    }
}

#[async_trait]
impl AccountSecretCache for DefaultAccountSecretCache {
    async fn set_user_keys(
        &self,
        unlocked_keys: &[PrivateKey],
    ) -> anyhow::Result<()> {
        let serialized = Self::serialize_private_keys(unlocked_keys)?;
        self.repository
            .set(Self::USER_KEYS_CACHE_KEY, serialized, vec![], )
            .await
    }

    async fn try_get_user_keys(
        &self,
    ) -> anyhow::Result<Option<Vec<PrivateKey>>> {
        let raw = self
            .repository
            .try_get(Self::USER_KEYS_CACHE_KEY, )
            .await?;
        match raw {
            Some(value) => Ok(Some(Self::deserialize_private_keys(&value)?)),
            None => Ok(None),
        }
    }

    async fn set_address_keys(
        &self,
        address_id: &str,
        unlocked_keys: &[PrivateKey],
    ) -> anyhow::Result<()> {
        let key = Self::get_address_keys_cache_key(address_id);
        let serialized = Self::serialize_private_keys(unlocked_keys)?;
        self.repository
            .set(&key, serialized, vec![], )
            .await
    }

    async fn try_get_address_keys(
        &self,
        address_id: &str,
    ) -> anyhow::Result<Option<Vec<PrivateKey>>> {
        let key = Self::get_address_keys_cache_key(address_id);
        let raw = self.repository.try_get(&key, ).await?;
        log::debug!("raw: {:?}", raw);
        match raw {
            Some(value) => Ok(Some(Self::deserialize_private_keys(&value)?)),
            None => Ok(None),
        }
    }
}

// trait ------------------------------------------

pub trait PublicKeyCache: Send + Sync {
    fn set_public_keys(&self, email_address: &str, public_keys: Vec<PublicKey>);
    fn try_get_public_keys(&self, email_address: &str) -> Option<Vec<PublicKey>>;
}

// impl -----------------------

pub struct DefaultPublicKeyCache {
    entries: DashMap<String, PublicKeyCacheEntry>,
}

impl DefaultPublicKeyCache {
    pub const NUMBER_OF_MINUTES_BEFORE_EXPIRATION: u64 = 30;

    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }
}

impl PublicKeyCache for DefaultPublicKeyCache {
    fn set_public_keys(&self, email_address: &str, public_keys: Vec<PublicKey>) {
        let entry = PublicKeyCacheEntry {
            expires_at: Instant::now() + Duration::from_secs(60 * Self::NUMBER_OF_MINUTES_BEFORE_EXPIRATION),
            public_keys,
        };
        self.entries.insert(email_address.to_string(), entry);
    }

    fn try_get_public_keys(&self, email_address: &str) -> Option<Vec<PublicKey>> {
        if let Some(entry) = self.entries.get(email_address) {
            if Instant::now() < entry.expires_at {
                return Some(entry.public_keys.clone());
            }
        }
        self.entries.remove(email_address);
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddressKeyCacheValue {
    address_id: String,
    address_key_id: String,
    is_active: bool,
    is_allowed_for_encryption: bool,
    is_allowed_for_verification: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddressCacheValue {
    address_id: String,
    order: i32,
    email_address: String,
    status: i32,
    primary_key_index: i32,
    keys: Vec<AddressKeyCacheValue>,
}

impl AddressCacheValue {
    fn from_address(address: &Address) -> Self {
        Self {
            address_id: address.address_id.clone(),
            order: address.order,
            email_address: address.email_address.clone(),
            status: address.status,
            primary_key_index: address.primary_key_index,
            keys: address
                .keys
                .iter()
                .map(|k| AddressKeyCacheValue {
                    address_id: k.address_id.clone(),
                    address_key_id: k.address_key_id.clone(),
                    is_active: k.is_active,
                    is_allowed_for_encryption: k.is_allowed_for_encryption,
                    is_allowed_for_verification: k.is_allowed_for_verification,
                })
                .collect(),
        }
    }

    fn into_address(self) -> Address {
        Address {
            address_id: self.address_id,
            order: self.order,
            email_address: self.email_address,
            status: self.status,
            keys: self
                .keys
                .into_iter()
                .map(|k| AddressKey {
                    address_id: k.address_id,
                    address_key_id: k.address_key_id,
                    is_active: k.is_active,
                    is_allowed_for_encryption: k.is_allowed_for_encryption,
                    is_allowed_for_verification: k.is_allowed_for_verification,
                })
                .collect(),
            primary_key_index: self.primary_key_index,
        }
    }
}

#[derive(Debug, Clone)]
struct PublicKeyCacheEntry {
    expires_at: Instant,
    public_keys: Vec<PublicKey>,
}

