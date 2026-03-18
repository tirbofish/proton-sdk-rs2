use crate::addresses::AddressesApiClient;
use crate::addresses::DefaultAddressesApiClient;
use crate::auth::TokenCredential;
use crate::client::ApiClient;
use crate::keys::DefaultKeysApiClient;
use crate::keys::KeysApiClient;
use crate::users::DefaultUsersApiClient;
use crate::users::UsersApiClient;
use std::sync::Arc;

pub trait AccountApiClients: Send + Sync {
    fn keys(&self) -> Arc<dyn KeysApiClient>;
    fn users(&self) -> Arc<dyn UsersApiClient>;
    fn addresses(&self) -> Arc<dyn AddressesApiClient>;
}

pub struct DefaultAccountApiClients {
    http_client: reqwest::Client,
    token_credential: Option<TokenCredential>,
}

impl DefaultAccountApiClients {
    pub fn new(http_client: reqwest::Client) -> DefaultAccountApiClients {
        DefaultAccountApiClients {
            http_client,
            token_credential: None,
        }
    }

    pub fn new_with_token_credential(
        http_client: reqwest::Client,
        token_credential: TokenCredential,
    ) -> DefaultAccountApiClients {
        DefaultAccountApiClients {
            http_client,
            token_credential: Some(token_credential),
        }
    }
}

impl AccountApiClients for DefaultAccountApiClients {
    fn keys(&self) -> Arc<dyn KeysApiClient> {
        if let Some(token_credential) = &self.token_credential {
            Arc::new(DefaultKeysApiClient::new_with_token_credential(
                self.http_client.clone(),
                token_credential.clone(),
            ))
        } else {
            Arc::new(ApiClient::create_keys_api_client(self.http_client.clone()))
        }
    }

    fn users(&self) -> Arc<dyn UsersApiClient> {
        if let Some(token_credential) = &self.token_credential {
            Arc::new(DefaultUsersApiClient::new_with_token_credential(
                self.http_client.clone(),
                token_credential.clone(),
            ))
        } else {
            Arc::new(ApiClient::create_users_api_client(self.http_client.clone()))
        }
    }

    fn addresses(&self) -> Arc<dyn AddressesApiClient> {
        if let Some(token_credential) = &self.token_credential {
            Arc::new(DefaultAddressesApiClient::new_with_token_credential(
                self.http_client.clone(),
                token_credential.clone(),
            ))
        } else {
            Arc::new(ApiClient::create_addresses_api_client(
                self.http_client.clone(),
            ))
        }
    }
}
