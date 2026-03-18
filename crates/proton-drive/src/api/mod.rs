pub mod attr;
pub mod block;
pub mod file;
pub mod folder;
pub mod links;
pub mod node;
pub mod revision;
pub mod share;
pub mod storage;
pub mod trash;
pub mod volumes;

use proton_sdk_rs2::auth::TokenCredential;
use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;
use std::sync::Arc;

pub trait DriveApiClientsFactory: Send + Sync {
    fn create(
        &self,
        default_api_http_client: ClientWithMiddleware,
        storage_api_http_client: ClientWithMiddleware,
        default_api_base_url: Url,
        storage_api_base_url: Url,
        token_credential: Option<TokenCredential>,
    ) -> Arc<dyn DriveApiClients>;
}

pub struct DefaultDriveApiClientsFactory;

impl DriveApiClientsFactory for DefaultDriveApiClientsFactory {
    fn create(
        &self,
        default_api_http_client: ClientWithMiddleware,
        storage_api_http_client: ClientWithMiddleware,
        default_api_base_url: Url,
        storage_api_base_url: Url,
        token_credential: Option<TokenCredential>,
    ) -> Arc<dyn DriveApiClients> {
        if let Some(token_credential) = token_credential {
            Arc::new(DefaultDriveApiClients::new_with_token_credential(
                default_api_http_client,
                storage_api_http_client,
                default_api_base_url,
                storage_api_base_url,
                token_credential,
            ))
        } else {
            Arc::new(DefaultDriveApiClients::new(
                default_api_http_client,
                storage_api_http_client,
                default_api_base_url,
                storage_api_base_url,
            ))
        }
    }
}
use crate::api::file::{DefaultFilesApiClient, FilesApiClient};
use crate::api::folder::{DefaultFoldersApiClient, FoldersApiClient};
use crate::api::links::{DefaultLinksApiClient, LinksApiClient};
use crate::api::share::{DefaultSharesApiClient, SharesApiClient};
use crate::api::storage::{DefaultStorageApiClient, StorageApiClient};
use crate::api::trash::{DefaultTrashApiClient, TrashApiClient};
use crate::api::volumes::{DefaultVolumesApiClient, VolumesApiClient};

use reqwest::Url;

pub trait DriveApiClients: Send + Sync {
    fn volumes(&self) -> Arc<dyn VolumesApiClient>;
    fn shares(&self) -> Arc<dyn SharesApiClient>;
    fn links(&self) -> Arc<dyn LinksApiClient>;
    fn folders(&self) -> Arc<dyn FoldersApiClient>;
    fn files(&self) -> Arc<dyn FilesApiClient>;
    fn storage(&self) -> Arc<dyn StorageApiClient>;
    fn trash(&self) -> Arc<dyn TrashApiClient>;
}

pub struct DefaultDriveApiClients {
    default_api_http_client: ClientWithMiddleware,
    storage_api_http_client: ClientWithMiddleware,
    default_api_base_url: Url,
    storage_api_base_url: Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultDriveApiClients {
    pub fn new(
        default_api_http_client: ClientWithMiddleware,
        storage_api_http_client: ClientWithMiddleware,
        default_api_base_url: Url,
        storage_api_base_url: Url,
    ) -> Self {
        Self {
            default_api_http_client,
            storage_api_http_client,
            default_api_base_url,
            storage_api_base_url,
            token_credential: None,
        }
    }

    pub fn new_with_token_credential(
        default_api_http_client: ClientWithMiddleware,
        storage_api_http_client: ClientWithMiddleware,
        default_api_base_url: Url,
        storage_api_base_url: Url,
        token_credential: TokenCredential,
    ) -> Self {
        Self {
            default_api_http_client,
            storage_api_http_client,
            default_api_base_url,
            storage_api_base_url,
            token_credential: Some(token_credential),
        }
    }
}

impl DriveApiClients for DefaultDriveApiClients {
    fn volumes(&self) -> Arc<dyn VolumesApiClient> {
        Arc::new(DefaultVolumesApiClient::new(
            self.default_api_http_client.clone(),
            self.default_api_base_url.clone(),
            self.token_credential.clone(),
        ))
    }

    fn shares(&self) -> Arc<dyn SharesApiClient> {
        Arc::new(DefaultSharesApiClient::new(
            self.default_api_http_client.clone(),
            self.default_api_base_url.clone(),
            self.token_credential.clone(),
        ))
    }

    fn links(&self) -> Arc<dyn LinksApiClient> {
        Arc::new(DefaultLinksApiClient::new(
            self.default_api_http_client.clone(),
            self.default_api_base_url.clone(),
            self.token_credential.clone(),
        ))
    }

    fn folders(&self) -> Arc<dyn FoldersApiClient> {
        Arc::new(DefaultFoldersApiClient::new(
            self.default_api_http_client.clone(),
            self.default_api_base_url.clone(),
            self.token_credential.clone(),
        ))
    }

    fn files(&self) -> Arc<dyn FilesApiClient> {
        Arc::new(DefaultFilesApiClient::new(
            self.default_api_http_client.clone(),
            self.default_api_base_url.clone(),
            self.token_credential.clone(),
        ))
    }

    fn storage(&self) -> Arc<dyn StorageApiClient> {
        Arc::new(DefaultStorageApiClient::new(
            self.default_api_http_client.clone(),
            self.storage_api_http_client.clone(),
            self.default_api_base_url.clone(),
            self.storage_api_base_url.clone(),
            self.token_credential.clone(),
        ))
    }

    fn trash(&self) -> Arc<dyn TrashApiClient> {
        Arc::new(DefaultTrashApiClient::new(
            self.default_api_http_client.clone(),
            self.default_api_base_url.clone(),
            self.token_credential.clone(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct ResponseCode(pub u32);

impl ResponseCode {
    pub const SUCCESS: ResponseCode = ResponseCode(1000);

    pub fn is_success(self) -> bool {
        self == Self::SUCCESS
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ApiResponse {
    pub code: ResponseCode,

    #[serde(rename = "Error")]
    pub error_message: Option<String>,
}

impl ApiResponse {
    pub fn is_success(&self) -> bool {
        self.code.is_success()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AggregateApiResponse<T> {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "Responses")]
    pub responses: Vec<T>,
}

impl<T> AggregateApiResponse<T> {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}
