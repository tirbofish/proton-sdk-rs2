use thiserror::Error;

#[derive(Debug, Error, Clone, serde::Serialize, serde::Deserialize)]
pub enum ProtonDriveError {
    #[error("API error: {0}")]
    ApiError(String),
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Cryptography error: {0}")]
    CryptoError(String),
    #[error("Not found")]
    NotFound,
    #[error("Unauthorized")]
    Unauthorized,
}
