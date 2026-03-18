pub mod cache;
pub mod client;
pub(crate) mod response;

use http::Uri;
use serde::{Deserialize, Deserializer};

use crate::{
    addresses::DefaultAddressesApiClient, auth::DefaultAuthenticationApiClient,
    keys::DefaultKeysApiClient, users::DefaultUsersApiClient,
};

pub trait ApiClientFactory {
    fn create_authentication_api_client(
        &self,
        http_client: reqwest::Client,
        refresh_redirect_uri: Uri,
    ) -> DefaultAuthenticationApiClient {
        DefaultAuthenticationApiClient::new(http_client, refresh_redirect_uri)
    }

    fn create_keys_api_client(&self, http_client: reqwest::Client) -> DefaultKeysApiClient {
        DefaultKeysApiClient::new(http_client)
    }

    fn create_users_api_client(&self, http_client: reqwest::Client) -> DefaultUsersApiClient {
        DefaultUsersApiClient::new(http_client)
    }

    fn create_addresses_api_client(
        &self,
        http_client: reqwest::Client,
    ) -> DefaultAddressesApiClient {
        DefaultAddressesApiClient::new(http_client)
    }
}

use crate::api::ResponseCode::CustomCode;
use crate::api::ResponseCode::ProtonDriveUnknown;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ApiResponse {
    #[serde(rename = "Code", alias = "code")]
    code: ResponseCode,
    #[serde(rename = "Error", alias = "error", default)]
    error_message: Option<String>,
}

impl ApiResponse {
    #[allow(dead_code)]
    fn is_success(&self) -> bool {
        self.code == ResponseCode::Success
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResponseCode {
    Unknown = 0,

    Unauthorized = 401,
    Forbidden = 403,
    RequestTimeout = 408,

    Success = 1000,
    MultipleResponses = 1001,
    InvalidRequirements = 2000,
    InvalidValue = 2001,
    InvalidEncryptedIdFormat = 2061,
    AlreadyExists = 2500,
    DoesNotExist = 2501,
    Timeout = 2503,
    IncompatibleState = 2511,
    InvalidApp = 5002,
    OutdatedApp = 5003,
    Offline = 7001,
    IncorrectLoginCredentials = 8002,

    /// Account is disabled
    AccountDeleted = 10002,

    /// Account is disabled due to abuse or fraud
    AccountDisabled = 10003,

    InvalidRefreshToken = 10013,

    /// Free account
    NoActiveSubscription = 22110,

    UnknownAddress = 33102,

    ProtonDriveUnknown = 200000,
    InsufficientQuota = ProtonDriveUnknown as isize + 1,
    InsufficientSpace = ProtonDriveUnknown as isize + 2,
    MaxFileSizeForFreeUser = ProtonDriveUnknown as isize + 3,
    TooManyChildren = ProtonDriveUnknown as isize + 300,

    CustomCode = 10000000,
    SocketError = CustomCode as isize + 1,
    SessionRefreshFailed = CustomCode as isize + 3,
    SrpError = CustomCode as isize + 4,
}

impl<'de> Deserialize<'de> for ResponseCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let code = i64::deserialize(deserializer)?;

        let mapped = match code {
            0 => Self::Unknown,
            401 => Self::Unauthorized,
            403 => Self::Forbidden,
            408 => Self::RequestTimeout,
            1000 => Self::Success,
            1001 => Self::MultipleResponses,
            2000 => Self::InvalidRequirements,
            2001 => Self::InvalidValue,
            2061 => Self::InvalidEncryptedIdFormat,
            2500 => Self::AlreadyExists,
            2501 => Self::DoesNotExist,
            2503 => Self::Timeout,
            2511 => Self::IncompatibleState,
            5002 => Self::InvalidApp,
            5003 => Self::OutdatedApp,
            7001 => Self::Offline,
            8002 => Self::IncorrectLoginCredentials,
            10002 => Self::AccountDeleted,
            10003 => Self::AccountDisabled,
            10013 => Self::InvalidRefreshToken,
            22110 => Self::NoActiveSubscription,
            33102 => Self::UnknownAddress,
            x if x == ProtonDriveUnknown as i64 => Self::ProtonDriveUnknown,
            x if x == Self::InsufficientQuota as i64 => Self::InsufficientQuota,
            x if x == Self::InsufficientSpace as i64 => Self::InsufficientSpace,
            x if x == Self::MaxFileSizeForFreeUser as i64 => Self::MaxFileSizeForFreeUser,
            x if x == Self::TooManyChildren as i64 => Self::TooManyChildren,
            x if x == CustomCode as i64 => Self::CustomCode,
            x if x == Self::SocketError as i64 => Self::SocketError,
            x if x == Self::SessionRefreshFailed as i64 => Self::SessionRefreshFailed,
            x if x == Self::SrpError as i64 => Self::SrpError,
            _ => Self::Unknown,
        };

        Ok(mapped)
    }
}
