use std::path::PathBuf;

use anyhow::{anyhow, Result};
use platform_dirs::AppDirs;

pub struct AppDataPaths {
    pub credentials_path: PathBuf,
    pub secrets_cache_path: PathBuf,
    pub history_path: PathBuf,
    pub cache_dir: PathBuf,
    pub settings_path: PathBuf,
}

pub fn resolve_paths() -> Result<AppDataPaths> {
    let app_dirs = AppDirs::new(Some("pdcli"), true)
        .ok_or_else(|| anyhow!("Unable to resolve appdata directories"))?;

    std::fs::create_dir_all(&app_dirs.config_dir)?;
    std::fs::create_dir_all(&app_dirs.data_dir)?;
    std::fs::create_dir_all(&app_dirs.cache_dir)?;

    Ok(AppDataPaths {
        credentials_path: app_dirs.config_dir.join("cred.ron"),
        secrets_cache_path: app_dirs.data_dir.join("cache.json"),
        history_path: app_dirs.data_dir.join("history.txt"),
        settings_path: app_dirs.config_dir.join("settings.toml"),
        cache_dir: app_dirs.cache_dir,
    })
}
