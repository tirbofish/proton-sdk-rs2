use std::time::Duration;
use thiserror::Error;

/// HTTP failure returned by a blob transfer endpoint.
///
/// Keeping the status code typed lets callers distinguish a transient service
/// failure from a permanent client error without parsing an error string.
#[derive(Debug, Error)]
#[error("blob transfer failed with HTTP status {status}: {message}")]
pub struct HttpTransferError {
    pub status: u16,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl HttpTransferError {
    pub fn new(
        status: reqwest::StatusCode,
        message: impl Into<String>,
        retry_after: Option<Duration>,
    ) -> Self {
        Self {
            status: status.as_u16(),
            message: message.into(),
            retry_after,
        }
    }

    pub fn status_code(&self) -> Option<reqwest::StatusCode> {
        reqwest::StatusCode::from_u16(self.status).ok()
    }

    /// Returns whether retrying this transfer is useful.
    pub fn is_retryable(&self) -> bool {
        matches!(self.status, 408 | 425 | 429) || self.status >= 500
    }

    pub fn is_expired_target(&self) -> bool {
        self.status == reqwest::StatusCode::NOT_FOUND.as_u16()
    }
}

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
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    Abort(String),
    #[error("{0}")]
    Connection(String),
    #[error("{0}")]
    RateLimited(String),
    #[error("{0}")]
    Server(String),
    #[error("{0}")]
    Unimplemented(String),
}

impl ProtonDriveError {
    pub fn abort() -> Self {
        Self::Abort("Operation aborted".into())
    }

    pub fn is_abort(&self) -> bool {
        matches!(self, Self::Abort(_))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Abort(_) => "AbortError",
            Self::Validation(_) => "ValidationError",
            Self::RateLimited(_) => "RateLimitedError",
            Self::Connection(_) => "ConnectionError",
            Self::Server(_) => "ServerError",
            Self::NotFound => "NotFound",
            Self::Unauthorized => "Unauthorized",
            Self::ApiError(_) => "ApiError",
            Self::InternalError(_) => "InternalError",
            Self::CryptoError(_) => "CryptoError",
            Self::Unimplemented(_) => "UnimplementedError",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    NotSigned,
    SignedAndInvalid,
    SignedAndValid,
}

pub fn get_verification_message(
    verified: VerificationStatus,
    verification_errors: Option<&[String]>,
    signature_type: Option<&str>,
    not_available_verification_keys: bool,
) -> String {
    if verified == VerificationStatus::NotSigned {
        return match signature_type {
            Some(kind) => format!("Missing signature for {kind}"),
            None => "Missing signature".into(),
        };
    }
    if not_available_verification_keys {
        return match signature_type {
            Some(kind) => format!("Verification keys for {kind} are not available"),
            None => "Verification keys are not available".into(),
        };
    }
    if let Some(errors) = verification_errors {
        let joined = errors.join(", ");
        return match signature_type {
            Some(kind) => format!("Signature verification for {kind} failed: {joined}"),
            None => format!("Signature verification failed: {joined}"),
        };
    }
    match signature_type {
        Some(kind) => format!("Signature verification for {kind} failed"),
        None => "Signature verification failed".into(),
    }
}

pub fn is_not_application_error(error: Option<&ProtonDriveError>) -> bool {
    matches!(
        error,
        Some(
            ProtonDriveError::Abort(_)
                | ProtonDriveError::Validation(_)
                | ProtonDriveError::RateLimited(_)
                | ProtonDriveError::Connection(_)
        )
    )
}

pub fn is_not_application_error_name(name: &str) -> bool {
    matches!(name, "AbortError" | "OfflineError" | "TimeoutError")
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct IntegrityException {
    pub message: String,
}

impl IntegrityException {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Error)]
#[error("content size mismatch: uploaded {uploaded} bytes, expected {expected} bytes")]
pub struct ContentSizeMismatchIntegrityException {
    pub uploaded: i64,
    pub expected: i64,
}

#[derive(Debug, Error)]
#[error("checksum mismatch")]
pub struct ChecksumMismatchIntegrityException {
    pub actual: Vec<u8>,
    pub expected: Vec<u8>,
}

#[derive(Debug, Error)]
#[error("too many requests")]
pub struct TooManyRequestsException {
    pub retry_after: Option<Duration>,
}

impl TooManyRequestsException {
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        Self {
            retry_after: parse_retry_after(headers),
        }
    }
}

pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    value.parse::<u64>().ok().map(Duration::from_secs)
}

/// C# RetryPolicy: `2^(attempt-2)` seconds plus up to 250ms jitter.
pub fn retry_backoff_delay(attempt: u32) -> Duration {
    let exp = 2u64.saturating_pow(attempt.saturating_sub(1).min(6));
    let base_ms = 500 * exp;
    let jitter_ms = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_millis())
        .unwrap_or(0)
        % 250) as u64;
    Duration::from_millis(base_ms + jitter_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_retry_after_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "12".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(12)));
    }

    #[test]
    fn checksum_mismatch_is_integrity_error() {
        let err = ChecksumMismatchIntegrityException {
            actual: vec![1],
            expected: vec![2],
        };
        assert_eq!(err.to_string(), "checksum mismatch");
    }

    #[test]
    fn retry_backoff_grows() {
        let first = retry_backoff_delay(1);
        let second = retry_backoff_delay(2);
        assert!(first >= Duration::from_millis(500));
        assert!(second >= Duration::from_millis(1000));
    }

    #[test]
    fn missing_or_invalid_retry_after_is_ignored() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
        headers.insert(reqwest::header::RETRY_AFTER, "later".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn content_size_mismatch_includes_both_sizes() {
        assert_eq!(
            ContentSizeMismatchIntegrityException {
                uploaded: 41,
                expected: 42,
            }
            .to_string(),
            "content size mismatch: uploaded 41 bytes, expected 42 bytes"
        );
    }

    #[test]
    fn validation_error_preserves_the_message() {
        assert_eq!(
            ProtonDriveError::Validation("Invalid URL".into()).to_string(),
            "Invalid URL"
        );
    }

    #[test]
    fn retry_backoff_is_bounded_at_the_thirty_two_second_step() {
        let delay = retry_backoff_delay(u32::MAX);
        assert!(delay >= Duration::from_secs(32));
        assert!(delay < Duration::from_millis(32_250));
    }

    #[test]
    fn get_verification_message_matches_typescript_cases() {
        use VerificationStatus::*;
        let errors = ["error1".to_string(), "error2".to_string()];
        let cases: [(
            VerificationStatus,
            Option<&[String]>,
            Option<&str>,
            bool,
            &str,
        ); 8] = [
            (
                NotSigned,
                None,
                Some("type"),
                false,
                "Missing signature for type",
            ),
            (NotSigned, None, None, false, "Missing signature"),
            (
                NotSigned,
                None,
                Some("type"),
                true,
                "Missing signature for type",
            ),
            (
                SignedAndInvalid,
                None,
                Some("type"),
                false,
                "Signature verification for type failed",
            ),
            (
                SignedAndInvalid,
                None,
                None,
                false,
                "Signature verification failed",
            ),
            (
                SignedAndInvalid,
                None,
                Some("type"),
                true,
                "Verification keys for type are not available",
            ),
            (
                SignedAndInvalid,
                None,
                None,
                true,
                "Verification keys are not available",
            ),
            (
                SignedAndInvalid,
                Some(errors.as_slice()),
                None,
                false,
                "Signature verification failed: error1, error2",
            ),
        ];
        for (status, errors, kind, missing_keys, expected) in cases {
            assert_eq!(
                get_verification_message(status, errors, kind, missing_keys),
                expected
            );
        }
    }

    #[test]
    fn is_not_application_error_matches_typescript() {
        assert!(is_not_application_error(Some(&ProtonDriveError::abort())));
        assert!(is_not_application_error(Some(
            &ProtonDriveError::Validation("x".into())
        )));
        assert!(is_not_application_error(Some(
            &ProtonDriveError::RateLimited("x".into())
        )));
        assert!(is_not_application_error(Some(
            &ProtonDriveError::Connection("x".into())
        )));
        assert!(!is_not_application_error(Some(
            &ProtonDriveError::InternalError("x".into())
        )));
        assert!(!is_not_application_error(None));
        assert!(is_not_application_error_name("AbortError"));
        assert!(is_not_application_error_name("OfflineError"));
        assert!(is_not_application_error_name("TimeoutError"));
        assert!(!is_not_application_error_name("Error"));
    }

    #[test]
    fn transfer_http_errors_classify_permanent_and_transient_statuses() {
        let permanent = HttpTransferError::new(reqwest::StatusCode::BAD_REQUEST, "bad", None);
        let transient =
            HttpTransferError::new(reqwest::StatusCode::SERVICE_UNAVAILABLE, "busy", None);
        let expired = HttpTransferError::new(reqwest::StatusCode::NOT_FOUND, "expired", None);

        assert!(!permanent.is_retryable());
        assert!(transient.is_retryable());
        assert!(expired.is_expired_target());
    }
}
