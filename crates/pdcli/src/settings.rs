use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Maximum number of entries in the entity cache before LRU eviction kicks in.
    /// `None` (default) means the cache grows without bound.
    pub entity_cache_max_size: Option<usize>,
    /// Maximum number of entries in the secret cache before LRU eviction kicks in.
    /// `None` (default) means the cache grows without bound.
    pub secret_cache_max_size: Option<usize>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            entity_cache_max_size: None,
            secret_cache_max_size: None,
        }
    }
}

impl Settings {
    pub fn load() -> anyhow::Result<Self> {
        let path = crate::app_paths::settings_path();
        if path.exists() {
            let s = std::fs::read_to_string(&path)?;
            Ok(toml::from_str(&s)?)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = crate::app_paths::settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string(self)?)?;
        Ok(())
    }
}
