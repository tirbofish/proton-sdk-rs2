//! Keeps the crypto time in sync with the API.
//!
//! Without it, keys and signatures are dated by the device clock. A device
//! clock ahead of the server produces keys from the future that the API
//! rejects as invalid. Like upstream's `updateServerTime`, the offset is shared
//! by every client in the process.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use proton_rpgp::{CheckUnixTime, UnixTime};

/// Server time minus device time, in seconds.
static OFFSET_SECONDS: AtomicI64 = AtomicI64::new(0);

fn device_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Records the server time reported by an API response.
pub fn update(server_unix_seconds: i64) {
    OFFSET_SECONDS.store(server_unix_seconds - device_now(), Ordering::Relaxed);
}

/// Server time from `x-pm-date`, falling back to `Date`, in Unix seconds.
fn server_time_from_headers(headers: &http::HeaderMap) -> Option<i64> {
    let parse = |name: &str| {
        let value = headers.get(name)?.to_str().ok()?;
        chrono::DateTime::parse_from_rfc2822(value)
            .or_else(|_| chrono::DateTime::parse_from_rfc3339(value))
            .ok()
    };
    parse("x-pm-date")
        .or_else(|| parse("date"))
        .map(|date| date.timestamp())
}

/// Updates the server time from `x-pm-date`, falling back to `Date`.
pub fn update_from_headers(headers: &http::HeaderMap) {
    if let Some(server_time) = server_time_from_headers(headers) {
        update(server_time);
    }
}

/// The current server time in Unix seconds.
pub fn now_unix_seconds() -> i64 {
    device_now() + OFFSET_SECONDS.load(Ordering::Relaxed)
}

/// The current server time, for dating new keys.
pub fn now() -> UnixTime {
    UnixTime::new(now_unix_seconds().max(0) as u64)
}

/// The current server time, for signing and encrypting.
pub fn check_now() -> CheckUnixTime {
    now().into()
}

/// Updates the server time from every API response.
pub struct ServerTimeMiddleware;

#[async_trait::async_trait]
impl reqwest_middleware::Middleware for ServerTimeMiddleware {
    async fn handle(
        &self,
        request: reqwest::Request,
        extensions: &mut http::Extensions,
        next: reqwest_middleware::Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        let response = next.run(request, extensions).await?;
        update_from_headers(response.headers());
        Ok(response)
    }
}

/// Wraps an HTTP client so its responses keep the crypto time in sync.
pub fn client_with_server_time(
    client: reqwest::Client,
) -> reqwest_middleware::ClientWithMiddleware {
    reqwest_middleware::ClientBuilder::new(client)
        .with(ServerTimeMiddleware)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_time_prefers_pm_date_then_date_and_ignores_invalid_values() {
        let mut headers = http::HeaderMap::new();
        headers.insert("date", "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap());
        assert_eq!(server_time_from_headers(&headers), Some(1_445_412_480));

        headers.insert(
            "x-pm-date",
            "Thu, 22 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(server_time_from_headers(&headers), Some(1_445_498_880));

        let mut invalid = http::HeaderMap::new();
        invalid.insert("date", "not a date".parse().unwrap());
        assert_eq!(server_time_from_headers(&invalid), None);
    }
}
