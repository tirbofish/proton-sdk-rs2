use std::path::Path;

/// Set file permissions to owner-only read/write (0600) on Unix systems.
/// This prevents other users on the system from reading sensitive files
/// such as cached credentials and secret key material.
#[cfg(unix)]
pub fn set_restricted_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
pub fn set_restricted_permissions(_path: &Path) -> anyhow::Result<()> {
    // On non-Unix platforms, file permission hardening is not yet implemented.
    // Consider using platform-specific ACLs in the future.
    Ok(())
}
