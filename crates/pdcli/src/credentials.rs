use std::path::PathBuf;

use platform_dirs::AppDirs;
use proton_sdk_rs2::ser::StoredCredentials;

const APP_NAME: &str = "pdcli";
const CRED_FILE: &str = "cred.ron";

fn cred_dir() -> PathBuf {
    AppDirs::new(Some(APP_NAME), false)
        .expect("failed to resolve platform config directory")
        .config_dir
}

fn cred_path() -> PathBuf {
    cred_dir().join(CRED_FILE)
}

pub fn load() -> Option<StoredCredentials> {
    let path = cred_path();
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return None,
    };
    match ron::from_str(&data) {
        Ok(cred) => {
            tracing::debug!(path = %path.display(), "loaded credentials");
            Some(cred)
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "corrupt credentials file");
            None
        }
    }
}

pub fn save(cred: &StoredCredentials) -> anyhow::Result<()> {
    let dir = cred_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(CRED_FILE);
    let data = ron::ser::to_string_pretty(cred, ron::ser::PrettyConfig::default())?;
    std::fs::write(&path, &data)?;
    tracing::debug!(path = %path.display(), "saved credentials");
    Ok(())
}

pub fn remove() {
    let path = cred_path();
    if std::fs::remove_file(&path).is_ok() {
        tracing::debug!(path = %path.display(), "removed credentials");
    }
}
