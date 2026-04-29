use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Configuration for thumbnail/preview process detection.
/// Controls which processes are allowed to read uncached file content
/// vs blocked (treated as thumbnailers that shouldn't trigger downloads).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbnailConfig {
    /// Processes explicitly allowed to read file content (viewers, editors).
    /// Matched by contains() against /proc/pid/exe.
    #[serde(default)]
    pub allowed_exes: Vec<String>,

    /// Processes to block from reading uncached file content.
    /// Matched by contains() against /proc/pid/exe.
    #[serde(default)]
    pub blocked_exes: Vec<String>,

    /// Process names (from /proc/pid/comm) that are always allowed.
    #[serde(default)]
    pub allowed_names: Vec<String>,

    /// Process names that should be blocked (thumbnailers, indexers).
    #[serde(default)]
    pub blocked_names: Vec<String>,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            allowed_exes: vec![
                // PDF/Document viewers
                "/papers".into(),
                "/evince".into(),
                "/okular".into(),
                "/zathura".into(),
                // Text editors
                "/gedit".into(),
                "/gnome-text-editor".into(),
                "/kate".into(),
                "/kwrite".into(),
                "/mousepad".into(),
                "/pluma".into(),
                "/xed".into(),
                // Image viewers
                "/loupe".into(),
                "/eog".into(),
                "/gwenview".into(),
                "/feh".into(),
                "/sxiv".into(),
                // Video players
                "/totem".into(),
                "/vlc".into(),
                "/mpv".into(),
                "/celluloid".into(),
                // Browsers
                "/firefox".into(),
                "/chromium".into(),
                "/chrome".into(),
                "/brave".into(),
                // Office
                "/libreoffice".into(),
                "/soffice".into(),
                // Code editors
                "/code".into(),
                "/codium".into(),
                "/sublime_text".into(),
                "/atom".into(),
            ],
            blocked_exes: vec![
                // File managers — pool threads read content for thumbnails
                "/nautilus".into(),
                "/dolphin".into(),
                "/thunar".into(),
                "/nemo".into(),
                "/pcmanfm".into(),
                "/caja".into(),
                "/spacefm".into(),
                // Thumbnailer paths
                "/libexec/".into(),
            ],
            allowed_names: vec![
                "papers".into(),
                "evince".into(),
                "gedit".into(),
                "gnome-text-ed".into(),
                "loupe".into(),
                "eog".into(),
                "totem".into(),
                "vlc".into(),
                "mpv".into(),
                "firefox".into(),
                "chromium".into(),
                "chrome".into(),
                "libreoffice".into(),
                "soffice".into(),
                "lowriter".into(),
                "localc".into(),
                "code".into(),
                "codium".into(),
            ],
            blocked_names: vec![
                // File managers
                "nautilus".into(),
                "dolphin".into(),
                "thunar".into(),
                "nemo".into(),
                "pcmanfm".into(),
                "caja".into(),
                // Thumbnail services
                "tumbler".into(),
                "tumblerd".into(),
                "tracker-extract".into(),
                "tracker-miner-f".into(),
                "tracker".into(),
            ],
        }
    }
}

impl ThumbnailConfig {
    /// Load from ~/.config/pdcli/thumbnails.toml, creating a default if missing.
    pub fn load() -> Self {
        let config_path = Self::config_path();

        if config_path.exists() {
            match std::fs::read_to_string(&config_path) {
                Ok(contents) => match toml::from_str(&contents) {
                    Ok(config) => return config,
                    Err(e) => {
                        tracing::warn!("failed to parse thumbnail config: {}, using defaults", e);
                    }
                },
                Err(e) => {
                    tracing::warn!("failed to read thumbnail config: {}, using defaults", e);
                }
            }
        } else {
            let default_config = Self::default();
            if let Err(e) = default_config.save() {
                tracing::warn!("failed to write default thumbnail config: {}", e);
            } else {
                tracing::info!(path = %config_path.display(), "created default thumbnail config");
            }
            return default_config;
        }

        Self::default()
    }

    fn save(&self) -> anyhow::Result<()> {
        let config_path = Self::config_path();
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        let with_header = format!(
            "# pdcli Thumbnail Configuration\n\
             #\n\
             # Controls which processes may trigger file downloads.\n\
             # Allowed processes can read uncached file content.\n\
             # Blocked processes get EACCES for uncached files (no download).\n\
             # Patterns are matched with contains() against the exe path or process name.\n\n\
             {contents}"
        );
        std::fs::write(&config_path, with_header)?;
        Ok(())
    }

    fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pdcli")
            .join("thumbnails.toml")
    }

    /// Check if a process (by pid) should be blocked from triggering downloads.
    /// Returns `true` if the process is a known thumbnailer/indexer.
    pub fn is_blocked_process(&self, pid: u32) -> bool {
        // Read process name from /proc/pid/comm
        let comm = std::fs::read_to_string(format!("/proc/{}/comm", pid))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        // Read exe path from /proc/pid/exe (symlink)
        let exe = std::fs::read_link(format!("/proc/{}/exe", pid))
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        // Check exe path first (more reliable)
        if !exe.is_empty() {
            let exe_lower = exe.to_lowercase();
            for pattern in &self.allowed_exes {
                if exe_lower.contains(&pattern.to_lowercase()) {
                    return false;
                }
            }
            for pattern in &self.blocked_exes {
                if exe_lower.contains(&pattern.to_lowercase()) {
                    tracing::debug!(pid, exe = %exe, "blocking thumbnailer (exe match)");
                    return true;
                }
            }
        }

        // Check process name
        if !comm.is_empty() {
            let comm_lower = comm.to_lowercase();
            for pattern in &self.allowed_names {
                if comm_lower.starts_with(&pattern.to_lowercase()) {
                    return false;
                }
            }
            for pattern in &self.blocked_names {
                if comm_lower.starts_with(&pattern.to_lowercase()) {
                    tracing::debug!(pid, comm = %comm, "blocking thumbnailer (name match)");
                    return true;
                }
            }
        }

        // Unknown process — allow by default
        false
    }
}
