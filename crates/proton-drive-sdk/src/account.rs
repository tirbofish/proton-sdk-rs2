use async_trait::async_trait;
use proton_rpgp::{PrivateKey, PublicKey};
use proton_sdk_rs2::account::ProtonAccountClient;
use proton_sdk_rs2::protobuf::Address;
use proton_sdk_rs2::session::ProtonAPISession;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct AddressId(String);

impl Default for AddressId {
    fn default() -> Self {
        Self(String::new())
    }
}

impl AddressId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct AddressKeyId(String);

impl Default for AddressKeyId {
    fn default() -> Self {
        Self(String::new())
    }
}

impl AddressKeyId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &str {
        &self.0
    }
}

#[async_trait]
pub trait AccountClient: Send + Sync {
    async fn get_address(&self, address_id: &AddressId) -> anyhow::Result<Address>;
    async fn get_default_address(&self) -> anyhow::Result<Address>;
    async fn get_address_primary_private_key(
        &self,
        address_id: &AddressId,
    ) -> anyhow::Result<PrivateKey>;
    async fn get_address_private_keys(
        &self,
        address_id: &AddressId,
    ) -> anyhow::Result<Vec<PrivateKey>>;
    async fn get_address_public_keys(&self, email_address: &str) -> anyhow::Result<Vec<PublicKey>>;
    async fn get_user_keys(&self) -> anyhow::Result<Vec<PrivateKey>>;
    async fn get_user_storage_info(&self) -> anyhow::Result<(i64, i64)>;
}

pub struct AccountClientAdapter {
    client: ProtonAccountClient,
}

impl AccountClientAdapter {
    pub fn new(session: &ProtonAPISession) -> Self {
        Self {
            client: ProtonAccountClient::new(session),
        }
    }
}

#[async_trait]
impl AccountClient for AccountClientAdapter {
    async fn get_address(&self, address_id: &AddressId) -> anyhow::Result<Address> {
        self.client.get_address(address_id.raw()).await
    }

    async fn get_default_address(&self) -> anyhow::Result<Address> {
        self.client.get_current_user_default_address().await
    }

    async fn get_address_primary_private_key(
        &self,
        address_id: &AddressId,
    ) -> anyhow::Result<PrivateKey> {
        self.client
            .get_address_primary_private_key(address_id.raw())
            .await
    }

    async fn get_address_private_keys(
        &self,
        address_id: &AddressId,
    ) -> anyhow::Result<Vec<PrivateKey>> {
        self.client.get_address_private_keys(address_id.raw()).await
    }

    async fn get_address_public_keys(&self, email_address: &str) -> anyhow::Result<Vec<PublicKey>> {
        self.client.get_address_public_keys(email_address).await
    }

    async fn get_user_keys(&self) -> anyhow::Result<Vec<PrivateKey>> {
        self.client.get_user_keys().await
    }

    async fn get_user_storage_info(&self) -> anyhow::Result<(i64, i64)> {
        self.client.get_user_storage_info().await
    }
}
