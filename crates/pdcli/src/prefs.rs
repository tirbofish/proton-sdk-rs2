use std::path::PathBuf;

use platform_dirs::AppDirs;
use serde::{Deserialize, Serialize};

use crate::mount;

const APP_NAME: &str = "pdcli";
const PREFS_FILE: &str = "pref.toml";

fn prefs_path() -> PathBuf {
    AppDirs::new(Some(APP_NAME), false)
        .expect("failed to resolve platform config directory")
        .config_dir
        .join(PREFS_FILE)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default = "mount::default_mount_path")]
    pub mount_path: PathBuf,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            mount_path: mount::default_mount_path(),
        }
    }
}

pub fn load() -> Preferences {
    let path = prefs_path();
    match std::fs::read_to_string(&path) {
        Ok(data) => match toml::from_str(&data) {
            Ok(prefs) => {
                tracing::debug!(path = %path.display(), "loaded preferences");
                prefs
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "corrupt preferences file, using defaults");
                Preferences::default()
            }
        },
        Err(_) => Preferences::default(),
    }
}

pub fn save(prefs: &Preferences) -> anyhow::Result<()> {
    let path = prefs_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let data = toml::to_string_pretty(prefs)?;
    std::fs::write(&path, data)?;
    tracing::debug!(path = %path.display(), "saved preferences");
    Ok(())
}
