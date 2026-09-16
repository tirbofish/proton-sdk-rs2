//! Linux systemd user-service integration for the pdcli daemon.
//!
//! Source installs use the running executable's absolute path. Distribution
//! packages install the matching static unit from `packaging/systemd`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

pub const SERVICE_NAME: &str = "pdcli.service";

/// Install a systemd user unit for the currently running pdcli executable.
pub fn install() -> anyhow::Result<PathBuf> {
    ensure_linux()?;
    let path = unit_path()?;
    let executable = std::env::current_exe().context("failed to resolve pdcli executable")?;
    if !executable.is_absolute() {
        bail!(
            "pdcli executable path is not absolute: {}",
            executable.display()
        );
    }

    let parent = path
        .parent()
        .context("systemd user unit path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    std::fs::write(&path, unit_contents(&executable))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// Remove the source-installed user unit.
pub fn uninstall() -> anyhow::Result<()> {
    ensure_linux()?;
    let path = unit_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

/// Reload the user's systemd manager after installing or removing a unit.
pub fn reload() -> anyhow::Result<()> {
    ensure_linux()?;
    run_systemctl(["daemon-reload"])
}

/// Enable and start the pdcli user service.
pub fn enable() -> anyhow::Result<()> {
    ensure_linux()?;
    run_systemctl(["daemon-reload"])?;
    run_systemctl(["enable", "--now", SERVICE_NAME])
}

/// Stop and disable the pdcli user service.
pub fn disable() -> anyhow::Result<()> {
    ensure_linux()?;
    run_systemctl(["disable", "--now", SERVICE_NAME])
}

/// Return `systemctl status` output. Inactive services are valid status, so
/// the command's non-zero exit status is not treated as an error.
pub fn status() -> anyhow::Result<String> {
    ensure_linux()?;
    let output = Command::new("systemctl")
        .args([
            "--user",
            "--no-pager",
            "--plain",
            "--full",
            "status",
            SERVICE_NAME,
        ])
        .output()
        .context("failed to run systemctl --user; is systemd installed?")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr.trim().to_owned()
    } else if stderr.trim().is_empty() {
        stdout.trim().to_owned()
    } else {
        format!("{}\n{}", stdout.trim(), stderr.trim())
    };
    if text.is_empty() {
        bail!("systemctl returned no status output");
    }
    Ok(text)
}

/// Return the unit path used by `install`.
pub fn unit_path() -> anyhow::Result<PathBuf> {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("XDG_CONFIG_HOME or HOME is required for a systemd user service")?;
    if !config_dir.is_absolute() {
        bail!("XDG_CONFIG_HOME must be an absolute path");
    }
    Ok(config_dir.join("systemd/user").join(SERVICE_NAME))
}

fn ensure_linux() -> anyhow::Result<()> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        bail!("systemd user services are only supported on Linux")
    }
}

fn run_systemctl<const N: usize>(args: [&str; N]) -> anyhow::Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("failed to run systemctl --user; is systemd installed?")?;
    if output.status.success() {
        return Ok(());
    }

    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if message.is_empty() {
        bail!("systemctl --user exited with {}", output.status);
    }
    bail!("systemctl --user failed: {message}")
}

fn unit_contents(executable: &Path) -> String {
    format!(
        "[Unit]\nDescription=pdcli filesystem daemon (unofficial Proton Drive client)\nWants=network-online.target\nAfter=network-online.target\n\n[Service]\nType=simple\nExecStart={} daemon --no-tray\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=30\n\n[Install]\nWantedBy=default.target\n",
        systemd_quote(executable)
    )
}

fn systemd_quote(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "$$")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::{systemd_quote, unit_contents};
    use std::path::Path;

    #[test]
    fn generated_unit_uses_absolute_executable_and_no_tray() {
        let unit = unit_contents(Path::new("/home/test user/.local/bin/pdcli"));
        assert!(unit.contains("ExecStart=\"/home/test user/.local/bin/pdcli\" daemon --no-tray"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn systemd_specifier_characters_are_escaped() {
        assert_eq!(
            systemd_quote(Path::new("/tmp/$build/100%/pdcli")),
            "\"/tmp/$$build/100%%/pdcli\""
        );
    }
}
