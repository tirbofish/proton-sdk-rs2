use console::style;

/// Maximum number of retries before giving up on a persistent upload.
pub(super) const MAX_UPLOAD_RETRIES: u32 = 3;

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

/// Check if an error message indicates a permanent failure that should not be retried.
pub(super) fn is_permanent_upload_error(error_msg: &str) -> bool {
    if is_transient_error(error_msg) {
        return false;
    }

    let permanent_patterns = [
        "API error 2500",
        "API error 2501",
        "API error 2000",
        "API error 2001",
        "API error 2011",
        "API error 2061",
        "API error 200001",
        "API error 200002",
        "API error 200003",
        "API error 200300",
        "API error 200301",
        "already exists",
        "not enough space",
        "quota exceeded",
        "permission denied",
    ];

    let error_lower = error_msg.to_lowercase();
    permanent_patterns
        .iter()
        .any(|p| error_lower.contains(&p.to_lowercase()))
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
