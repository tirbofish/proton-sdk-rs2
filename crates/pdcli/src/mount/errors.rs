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
    
    // Draft revision errors (2500) are recoverable - they're handled by clearing the draft and retrying
    if is_draft_revision_error(error_msg) {
        return false;
    }
    
    // Stale revision errors (2511, 2061) are recoverable - refresh revision_uid and retry
    if is_stale_revision_error(error_msg) {
        return false;
    }

    let permanent_patterns = [
        "API error 2501",
        "API error 2000",
        "API error 2001",
        "API error 2011",
        "API error 200001",
        "API error 200002",
        "API error 200003",
        "API error 200300",
        "API error 200301",
        "not enough space",
        "quota exceeded",
        "permission denied",
    ];

    // Also check code-based format: "code 2501" etc
    let permanent_codes = ["2501", "2000", "2001", "2011", "200001", "200002", "200003", "200300", "200301"];
    
    let error_lower = error_msg.to_lowercase();
    
    // Check pattern-based
    if permanent_patterns.iter().any(|p| error_lower.contains(&p.to_lowercase())) {
        return true;
    }
    
    // Check code-based for "code XXXX" format
    if let Some(code_idx) = error_lower.find("code ") {
        let code_start = code_idx + 5;
        let code_end = error_lower[code_start..].find(|c: char| !c.is_ascii_digit()).map_or(error_lower.len(), |i| code_start + i);
        let code = &error_lower[code_start..code_end];
        if permanent_codes.contains(&code) {
            return true;
        }
    }
    
    // Check other patterns
    error_lower.contains("not enough space") 
        || error_lower.contains("quota exceeded") 
        || error_lower.contains("permission denied")
}

/// Check if an error is a "draft revision already exists" error (2500).
/// These can be recovered by deleting the stale draft and retrying.
pub(super) fn is_draft_revision_error(error_msg: &str) -> bool {
    let error_lower = error_msg.to_lowercase();
    error_lower.contains("2500") 
        || (error_lower.contains("draft") && error_lower.contains("already exists"))
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
