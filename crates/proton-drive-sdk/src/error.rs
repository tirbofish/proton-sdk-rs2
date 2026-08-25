use std::time::Duration;
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
    #[error("{0}")]
    Validation(String),
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
}
