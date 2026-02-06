pub(crate) mod response;

use crate::api::ResponseCode::ProtonDriveUnknown;
use crate::api::ResponseCode::CustomCode;

pub struct ApiResponse {
    code: ResponseCode,
    error_message: Option<String>,
}

impl ApiResponse {
    fn is_success(&self) -> bool {
        self.code == ResponseCode::Success
    }
}

#[derive(PartialEq)]
pub enum ResponseCode
{
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

