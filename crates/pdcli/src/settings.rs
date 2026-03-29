use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexingMethod {
    /// Eagerly index all files on startup; fast lookups after init but slow to start.
    IndexOnInit,
    /// Index a directory only when it is first listed; startup is instant but first `ls` may be slow.
    IndexOnDemand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub indexing_method: IndexingMethod,
}

impl Default for Settings {
    fn default() -> Self {
        Self { indexing_method: IndexingMethod::IndexOnDemand }
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
