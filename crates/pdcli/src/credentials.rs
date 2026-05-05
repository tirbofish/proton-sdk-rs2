use std::path::PathBuf;

use platform_dirs::AppDirs;
use proton_sdk_rs2::{ser::StoredCredentials, session::ProtonAPISession};

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

pub fn save_session_tokens_on_refresh(session: &ProtonAPISession) {
    let mut refreshed = session.token_credential.subscribe_tokens_refreshed();
    let session_id = session.session_id.raw().clone();
    let username = session.username.clone();
    let user_id = session.user_id.raw().clone();
    let scopes = session.scopes.clone();
    let is_waiting_for_second_factor_code = session.is_waiting_for_second_factor_code;
    let password_mode = session.password_mode;

    tokio::spawn(async move {
        while let Ok((access_token, refresh_token)) = refreshed.recv().await {
            let cred = StoredCredentials::new(
                session_id.clone(),
                username.clone(),
                user_id.clone(),
                access_token,
                refresh_token,
                scopes.clone(),
                is_waiting_for_second_factor_code,
                password_mode,
            );
            if let Err(e) = save(&cred) {
                tracing::warn!(error = %e, "failed to persist refreshed session tokens");
            }
        }
    });
}

pub fn remove() {
    let path = cred_path();
    if std::fs::remove_file(&path).is_ok() {
        tracing::debug!(path = %path.display(), "removed credentials");
    }
}
