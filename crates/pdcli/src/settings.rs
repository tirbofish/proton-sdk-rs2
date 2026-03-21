use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use anyhow::Result;

/// Indexing mode for folder traversal
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IndexMode {
    /// Index folders on demand (when accessing a folder not in local database)
    /// May be slow on initial folder loads but faster initialization
    IndexOnDemand,
    /// Index all folders during initialization
    /// Takes time during startup but provides better performance afterward
    IndexOnInit,
}

impl std::fmt::Display for IndexMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexMode::IndexOnDemand => write!(f, "IndexOnDemand"),
            IndexMode::IndexOnInit => write!(f, "IndexOnInit"),
        }
    }
}

/// Application settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Indexing settings
    #[serde(default)]
    pub indexing: IndexingSettings,
    /// Mounting settings
    #[serde(default)]
    pub mounting: MountingSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingSettings {
    /// Mode for indexing folders
    #[serde(default = "default_index_mode")]
    pub mode: IndexMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountingSettings {
    /// Cache size in GB for mounted filesystem
    #[serde(default = "default_cache_size")]
    pub cache_size_gb: f32,
}

fn default_index_mode() -> IndexMode {
    IndexMode::IndexOnInit
}

fn default_cache_size() -> f32 {
    5.0
}

impl Default for IndexingSettings {
    fn default() -> Self {
        Self {
            mode: default_index_mode(),
        }
    }
}

impl Default for MountingSettings {
    fn default() -> Self {
        Self {
            cache_size_gb: default_cache_size(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            indexing: IndexingSettings::default(),
            mounting: MountingSettings::default(),
        }
    }
}

impl Settings {
    /// Load settings from a file, or create default if file doesn't exist
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            let content = fs::read_to_string(path)?;
            let settings = toml::from_str(&content)?;
            Ok(settings)
        } else {
            Ok(Self::default())
        }
    }

    /// Save settings to a file
    pub fn save(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self)?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Display current settings
    pub fn display(&self) {
        println!("\n{}", " Current Settings ".bold().on_blue());
        println!("\n{}", " Indexing".bold());
        println!("  Mode: {} (default: IndexOnInit)", self.indexing.mode);
        println!("    • IndexOnDemand: Index folders on-demand (faster init, slower first access)");
        println!("    • IndexOnInit: Index all folders at startup (slower init, faster access)");
        println!("\n{}", " Mounting".bold());
        println!("  Cache Size: {:.1} GB (default: 5.0 GB)", self.mounting.cache_size_gb);
        println!("    • Size for preallocated mount cache");
        println!();
    }
}

// Import for color formatting
use crossterm::style::Stylize;
