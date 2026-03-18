use crate::api::cache::{AccountClientCache, DefaultAccountClientCache};
use crate::api::client::{AccountApiClients, DefaultAccountApiClients};
use crate::protobuf::{Address, AddressKey};
use crate::session::ProtonAPISession;
use proton_rpgp::{
    AsPublicKeyRef, DataEncoding, Decryptor, ExternalDetachedSignature, PrivateKey, PublicKey,
};
use std::sync::Arc;

pub struct ProtonAccountClient {
    pub api: Arc<dyn AccountApiClients>,
    pub cache: Arc<dyn AccountClientCache>,
}

impl ProtonAccountClient {
    pub fn new(session: &ProtonAPISession) -> Self {
        Self {
            api: Arc::new(DefaultAccountApiClients::new_with_token_credential(
                session.http_client.clone(),
                session.token_credential.clone(),
            )),
            cache: Arc::new(DefaultAccountClientCache::new(
                session.client_config.entity_cache_repository.clone(),
                session.client_config.secret_cache_repository.clone(),
                session.session_secret_cache.clone(),
            )),
        }
    }

    pub async fn get_address(&self, address_id: &str) -> anyhow::Result<Address> {
        if let Some(address) = self.cache.entities().try_get_address(address_id).await? {
            return Ok(address);
        }

        let response = self.api.addresses().get_address(address_id).await?;

        let user_keys = self.get_user_keys().await?;

        let address = self
            .convert_from_address_dto(response.address, &user_keys)
            .await?;
        self.cache.entities().set_address(&address).await?;
        Ok(address)
    }

    pub async fn get_current_user_addresses(&self) -> anyhow::Result<Vec<Address>> {
        if let Some(addresses) = self
            .cache
            .entities()
            .try_get_current_user_addresses()
            .await?
        {
            return Ok(addresses);
        }

        let response = self.api.addresses().get_addresses().await?;

        let user_keys = self.get_user_keys().await?;

        let mut addresses = Vec::new();
        for dto in response.addresses {
            match self.convert_from_address_dto(dto, &user_keys).await {
                Ok(address) => addresses.push(address),
                Err(error) => {
                    log::warn!("Failed to load address: {error}");
                }
            }
        }

        self.cache
            .entities()
            .set_current_user_addresses(&addresses)
            .await?;

        Ok(addresses)
    }

    pub async fn get_current_user_default_address(&self) -> anyhow::Result<Address> {
        let mut addresses = self.get_current_user_addresses().await?;
        if addresses.is_empty() {
            anyhow::bail!("User has no address")
        }
        addresses.sort_by_key(|a| a.order);
        Ok(addresses.remove(0))
    }

    pub async fn get_address_private_keys(
        &self,
        address_id: &str,
    ) -> anyhow::Result<Vec<PrivateKey>> {
        if let Some(keys) = self
            .cache
            .secrets()
            .try_get_address_keys(address_id)
            .await?
        {
            log::debug!("Located keys");
            return Ok(keys);
        }

        log::debug!("Address id: {:?}", address_id);
        let _ = self.get_address(address_id).await?;

        if let Some(keys) = self
            .cache
            .secrets()
            .try_get_address_keys(address_id)
            .await?
        {
            return Ok(keys);
        }

        anyhow::bail!("Could not get address keys for address {address_id}")
    }

    pub async fn get_address_primary_private_key(
        &self,
        address_id: &str,
    ) -> anyhow::Result<PrivateKey> {
        let address = self.get_address(address_id).await?;
        let keys = self.get_address_private_keys(address_id).await?;
        let index = usize::try_from(address.primary_key_index).unwrap_or(0);
        keys.get(index)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Address primary key index out of bounds"))
    }

    pub async fn get_address_private_key(
        &self,
        address_id: &str,
        index: usize,
    ) -> anyhow::Result<PrivateKey> {
        let keys = self.get_address_private_keys(address_id).await?;
        keys.get(index)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Address key index out of bounds"))
    }

    pub async fn get_address_public_keys(
        &self,
        email_address: &str,
    ) -> anyhow::Result<Vec<PublicKey>> {
        if let Some(cached) = self.cache.public_keys().try_get_public_keys(email_address) {
            return Ok(cached);
        }

        let response = self
            .api
            .keys()
            .get_active_public_keys(email_address.to_string())
            .await?;

        let mut keys = Vec::new();
        for key in response.address_public_keys {
            if let Some(armored) = key.public_key {
                if let Ok(public_key) = PublicKey::import(armored.as_bytes(), DataEncoding::Auto) {
                    keys.push(public_key);
                }
            }
        }

        self.cache
            .public_keys()
            .set_public_keys(email_address, keys.clone());

        Ok(keys)
    }

    pub async fn get_user_keys(&self) -> anyhow::Result<Vec<PrivateKey>> {
        if let Some(keys) = self.cache.secrets().try_get_user_keys().await? {
            return Ok(keys);
        }

        let response = self.api.users().get_user().await?;

        let mut unlocked = Vec::new();
        let mut active_key_found = false;

        for key in response
            .user
            .ok_or(anyhow::anyhow!("Unable to get keys from user"))?
            .keys
        {
            if !key.is_active {
                continue;
            }

            active_key_found = true;

            if let Ok(unlocked_user_key) =
                PrivateKey::import_unlocked(key.private_key.as_bytes(), DataEncoding::Auto)
            {
                unlocked.push(unlocked_user_key);
                continue;
            }

            let Some(passphrase) = self
                .cache
                .session_secrets()
                .try_get_account_key_passphrase(&key.id)
                .await?
            else {
                log::warn!("Unable to locate passphrase for user key {:?}", key.id);
                continue;
            };

            log::debug!("Passphrase: {:?}", String::from_utf8(passphrase.to_vec()));

            let unlocked_user_key =
                PrivateKey::import(key.private_key.as_bytes(), &passphrase, DataEncoding::Auto)?;
            unlocked.push(unlocked_user_key);
        }

        if unlocked.is_empty() {
            anyhow::bail!(
                "{}",
                if active_key_found {
                    format!("At least one active user key exists, but none could be unlocked")
                } else {
                    "No active user key found".to_string()
                }
            )
        }

        self.cache.secrets().set_user_keys(&unlocked).await?;

        Ok(unlocked)
    }

    async fn convert_from_address_dto(
        &self,
        dto: crate::addresses::AddressDto,
        user_keys: &[PrivateKey],
    ) -> anyhow::Result<Address> {
        let mut keys: Vec<AddressKey> = Vec::new();
        let mut unlocked = Vec::new();
        let mut primary_key_index: Option<i32> = None;

        for key_dto in dto.keys {
            if !key_dto.is_active {
                continue;
            }

            let current_index = i32::try_from(keys.len()).unwrap_or(i32::MAX);
            keys.push(AddressKey {
                address_id: dto.id.clone(),
                address_key_id: key_dto.id.clone(),
                is_active: key_dto.is_active,
                is_allowed_for_encryption: (key_dto.flags & 1) != 0,
                is_allowed_for_verification: (key_dto.flags & 2) != 0,
            });

            if key_dto.is_primary {
                primary_key_index = Some(current_index);
            }

            let imported = if let (Some(token), Some(signature)) =
                (key_dto.token.as_ref(), key_dto.signature.as_ref())
            {
                let passphrase =
                    Self::get_address_key_token_passphrase(token, signature, user_keys)?;
                PrivateKey::import(
                    key_dto.private_key.as_bytes(),
                    passphrase.as_slice(),
                    DataEncoding::Auto,
                )
            } else {
                let passphrase = self
                    .cache
                    .session_secrets()
                    .try_get_account_key_passphrase(&key_dto.id)
                    .await?;

                let Some(passphrase) = passphrase else {
                    log::warn!("No passphrase found for address key {}", key_dto.id);
                    continue;
                };

                PrivateKey::import(
                    key_dto.private_key.as_bytes(),
                    passphrase.as_slice(),
                    DataEncoding::Auto,
                )
            };

            match imported {
                Ok(key) => unlocked.push(key),
                Err(error) => {
                    log::warn!("Failed to import address key {}: {error}", key_dto.id);
                }
            }
        }

        let primary_key_index = primary_key_index.unwrap_or(0);

        if !unlocked.is_empty() {
            self.cache
                .secrets()
                .set_address_keys(&dto.id, &unlocked)
                .await?;
        }

        Ok(Address {
            address_id: dto.id,
            order: dto.order,
            email_address: dto.email,
            status: dto.status,
            keys,
            primary_key_index,
        })
    }

    fn get_address_key_token_passphrase(
        token: &str,
        signature: &str,
        user_keys: &[PrivateKey],
    ) -> anyhow::Result<Vec<u8>> {
        if user_keys.is_empty() {
            anyhow::bail!("No user keys available for address key token decryption")
        }

        let decrypted = Decryptor::default()
            .with_decryption_keys(user_keys.iter())
            .with_verification_keys(user_keys.iter().map(|key| key.as_public_key()))
            .with_external_detached_signature(ExternalDetachedSignature::new_unencrypted(
                signature.as_bytes(),
                DataEncoding::Auto,
            ))
            .decrypt(token.as_bytes(), DataEncoding::Auto)?;

        if !decrypted.verification_succeeded() {
            anyhow::bail!("Invalid account address key passphrase signature")
        }

        Ok(decrypted.data)
    }
}
