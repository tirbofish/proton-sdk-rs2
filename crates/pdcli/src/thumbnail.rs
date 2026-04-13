use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Configuration for thumbnail/preview process detection.
/// This allows users to customize which processes are allowed to read file content
/// vs blocked (treated as thumbnailers that shouldn't trigger downloads).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbnailConfig {
    /// Processes explicitly allowed to read file content (viewers, editors).
    /// These are checked by executable path (e.g., "/usr/bin/papers").
    #[serde(default)]
    pub allowed_exes: Vec<String>,
    
    /// Processes to block from reading uncached file content.
    /// These are checked by executable path (e.g., "/usr/bin/nautilus").
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
                "/papers".to_string(),
                "/evince".to_string(),
                "/okular".to_string(),
                "/zathura".to_string(),
                // Text editors
                "/gedit".to_string(),
                "/gnome-text-editor".to_string(),
                "/kate".to_string(),
                "/kwrite".to_string(),
                "/mousepad".to_string(),
                "/pluma".to_string(),
                "/xed".to_string(),
                // Image viewers
                "/loupe".to_string(),
                "/eog".to_string(),
                "/gwenview".to_string(),
                "/feh".to_string(),
                "/sxiv".to_string(),
                // Video players
                "/totem".to_string(),
                "/vlc".to_string(),
                "/mpv".to_string(),
                "/celluloid".to_string(),
                // Browsers
                "/firefox".to_string(),
                "/chromium".to_string(),
                "/chrome".to_string(),
                "/brave".to_string(),
                // Office
                "/libreoffice".to_string(),
                "/soffice".to_string(),
                // Code editors
                "/code".to_string(),
                "/codium".to_string(),
                "/sublime_text".to_string(),
                "/atom".to_string(),
            ],
            blocked_exes: vec![
                // File managers - their pool threads read content for thumbnails
                "/nautilus".to_string(),
                "/dolphin".to_string(),
                "/thunar".to_string(),
                "/nemo".to_string(),
                "/pcmanfm".to_string(),
                "/caja".to_string(),
                "/spacefm".to_string(),
                // Thumbnailer paths
                "/libexec/".to_string(),
            ],
            allowed_names: vec![
                "papers".to_string(),
                "evince".to_string(),
                "gedit".to_string(),
                "gnome-text-ed".to_string(),
                "loupe".to_string(),
                "eog".to_string(),
                "totem".to_string(),
                "vlc".to_string(),
                "mpv".to_string(),
                "firefox".to_string(),
                "chromium".to_string(),
                "chrome".to_string(),
                "libreoffice".to_string(),
                "soffice".to_string(),
                "lowriter".to_string(),
                "localc".to_string(),
                "code".to_string(),
                "codium".to_string(),
            ],
            blocked_names: vec![
                // File managers
                "nautilus".to_string(),
                "dolphin".to_string(),
                "thunar".to_string(),
                "nemo".to_string(),
                "pcmanfm".to_string(),
                "caja".to_string(),
                // Thumbnail services
                "tumbler".to_string(),
                "tumblerd".to_string(),
                "tracker-extract".to_string(),
                "tracker-miner-fs".to_string(),
                "tracker".to_string(),
            ],
        }
    }
}

impl ThumbnailConfig {
    /// Load the thumbnail config from ~/.config/pdcli/thumbnails.toml
    /// Creates a default config file if it doesn't exist.
    pub fn load() -> Self {
        let config_path = Self::config_path();
        
        if config_path.exists() {
            match std::fs::read_to_string(&config_path) {
                Ok(contents) => {
                    match toml::from_str(&contents) {
                        Ok(config) => {
                            tracing::debug!("Loaded thumbnail config from {:?}", config_path);
                            return config;
                        }
                        Err(e) => {
                            tracing::warn!("Failed to parse thumbnail config: {}. Using defaults.", e);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read thumbnail config: {}. Using defaults.", e);
                }
            }
        } else {
            // Create default config file
            let default_config = Self::default();
            if let Err(e) = default_config.save() {
                tracing::warn!("Failed to create default thumbnail config: {}", e);
            } else {
                tracing::info!("Created default thumbnail config at {:?}", config_path);
            }
            return default_config;
        }
        
        Self::default()
    }
    
    /// Save the config to disk.
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path();
        
        // Ensure parent directory exists
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        let contents = toml::to_string_pretty(self)?;
        
        // Add helpful comments
        let with_comments = format!(
r#"# pdcli Thumbnail Configuration
# 
# This file controls which processes are allowed to read file content
# vs blocked (treated as thumbnailers that shouldn't trigger downloads).
#
# Processes are identified by:
# - Executable path (from /proc/pid/exe) - more reliable
# - Process name (from /proc/pid/comm) - fallback, may be truncated to 15 chars
#
# Allowed processes can read file content and trigger downloads.
# Blocked processes will get "Permission denied" for uncached files.
#
# Patterns are matched with contains() - "/papers" matches "/usr/bin/papers"

{}"#, contents);
        
        std::fs::write(&config_path, with_comments)?;
        Ok(())
    }
    
    /// Get the config file path.
    fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pdcli")
            .join("thumbnails.toml")
    }
    
    /// Check if an executable path is explicitly allowed.
    pub fn is_exe_allowed(&self, exe: &str) -> Option<bool> {
        let exe_lower = exe.to_lowercase();
        
        // Check allowed first
        for pattern in &self.allowed_exes {
            if exe_lower.contains(&pattern.to_lowercase()) {
                return Some(true);
            }
        }
        
        // Check blocked
        for pattern in &self.blocked_exes {
            if exe_lower.contains(&pattern.to_lowercase()) {
                return Some(false);
            }
        }
        
        None // Not explicitly configured
    }
    
    /// Check if a process name is explicitly allowed.
    pub fn is_name_allowed(&self, name: &str) -> Option<bool> {
        let name_lower = name.to_lowercase();
        
        // Check allowed first
        for pattern in &self.allowed_names {
            let pattern_lower = pattern.to_lowercase();
            if name_lower.starts_with(&pattern_lower) || name_lower.contains(&pattern_lower) {
                return Some(true);
            }
        }
        
        // Check blocked
        for pattern in &self.blocked_names {
            let pattern_lower = pattern.to_lowercase();
            if name_lower.starts_with(&pattern_lower) || name_lower.contains(&pattern_lower) {
                return Some(false);
            }
        }
        
        None // Not explicitly configured
    }
}