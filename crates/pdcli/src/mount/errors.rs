use console::style;

/// Check if an error message indicates a transient/network failure that should be retried.
pub(super) fn is_transient_error(error_msg: &str) -> bool {
    let transient_patterns = [
        "error sending request",
        "connection refused",
        "connection reset",
        "connection closed",
        "timed out",
        "timeout",
        "temporarily unavailable",
        "service unavailable",
        "503",
        "502",
        "504",
        "network",
        "dns",
        "resolve",
        "socket",
        "broken pipe",
        "connection aborted",
        "API error 429",
        "too many requests",
    ];

    let error_lower = error_msg.to_lowercase();
    transient_patterns.iter().any(|p| error_lower.contains(p))
}

/// Check if an error indicates a stale revision reference.
/// These can be recovered by re-fetching the current revision_uid from server.
/// Includes:
/// - 2511: "Current revision is no longer up to date"
/// - 2061: "InvalidEncryptedIdFormat" (can happen when revision_uid is stale)
pub(super) fn is_stale_revision_error(error_msg: &str) -> bool {
    let error_lower = error_msg.to_lowercase();
    error_lower.contains("2511") 
        || error_lower.contains("2061")
        || error_lower.contains("no longer up to date")
        || error_lower.contains("invalid encrypted id")
}

/// Classify a download error and return a human-readable description and styled icon.
pub(super) fn classify_download_error(
    error_msg: &str,
) -> (&'static str, console::StyledObject<&'static str>) {
    let error_lower = error_msg.to_lowercase();

    if error_lower.contains("invalid mdc")
        || error_lower.contains("mdc mismatch")
        || error_lower.contains("decrypt")
        || error_lower.contains("decryption")
        || error_lower.contains("integrity")
    {
        return ("corrupted - cannot decrypt", style("✗").red());
    }

    if error_lower.contains("session key")
        || error_lower.contains("wrong key")
        || error_lower.contains("key mismatch")
    {
        return ("key error - cannot decrypt", style("✗").red());
    }

    if error_lower.contains("signature") || error_lower.contains("verification failed") {
        return ("signature invalid", style("✗").red());
    }

    if is_transient_error(&error_lower) {
        return ("network error", style("⚠").yellow());
    }

    if error_lower.contains("not found")
        || error_lower.contains("404")
        || error_lower.contains("block")
    {
        return ("file missing from storage", style("✗").red());
    }

    if error_lower.contains("unauthorized")
        || error_lower.contains("forbidden")
        || error_lower.contains("401")
        || error_lower.contains("403")
    {
        return ("access denied", style("✗").red());
    }

    ("download error", style("✗").red())
}
