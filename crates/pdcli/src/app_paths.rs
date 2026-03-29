use std::path::PathBuf;

pub fn app_dir() -> PathBuf {
    platform_dirs::AppDirs::new(Some("pdcli"), false)
        .map(|d| d.data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn cred_path() -> PathBuf {
    app_dir().join("credentials.ron")
}

pub fn cache_path() -> PathBuf {
    app_dir().join("cache.json")
}

pub fn db_path() -> PathBuf {
    app_dir().join("index.db")
}

pub fn settings_path() -> PathBuf {
    platform_dirs::AppDirs::new(Some("pdcli"), false)
        .map(|d| d.config_dir.join("settings.toml"))
        .unwrap_or_else(|| PathBuf::from("settings.toml"))
}
