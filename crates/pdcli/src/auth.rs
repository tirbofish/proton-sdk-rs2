use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use dialoguer::{Input, Password, theme::ColorfulTheme};
use indicatif::ProgressBar;
use proton_sdk_rs2::{
    PasswordMode, SessionId, UserId,
    client::ProtonClientOptions,
    session::{ProtonAPISession, ProtonSessionOptions},
};
use ron::ser::{PrettyConfig, to_string_pretty};
use std::time::Duration;

use crate::app_paths::resolve_paths;
use crate::file_cache::FileCacheRepository;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredCredentials {
    session_id: String,
    username: String,
    user_id: String,
    access_token: String,
    refresh_token: String,
    scopes: Vec<String>,
    is_waiting_for_second_factor_code: bool,
    password_mode: PasswordMode,
}

fn credentials_path() -> PathBuf {
    resolve_paths()
        .map(|p| p.credentials_path)
        .unwrap_or_else(|_| PathBuf::from("cred.ron"))
}

fn cache_path() -> PathBuf {
    resolve_paths()
        .map(|p| p.secrets_cache_path)
        .unwrap_or_else(|_| PathBuf::from("cache.json"))
}

fn load_cache_repository() -> Result<Arc<FileCacheRepository>> {
    Ok(Arc::new(FileCacheRepository::load(cache_path())?))
}

fn load_credentials(path: &Path) -> Result<Option<StoredCredentials>> {
    match fs::read_to_string(path) {
        Ok(content) => match ron::from_str::<StoredCredentials>(&content) {
            Ok(stored) => Ok(Some(stored)),
            Err(error) => {
                println!("Ignoring invalid cred.ron ({error}), creating a new session");
                Ok(None)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn persist_credentials(path: &Path, session: &ProtonAPISession) -> Result<()> {
    let (access_token, refresh_token) = session.token_credential.get_tokens().await?;

    let stored = StoredCredentials {
        session_id: session.session_id.to_string(),
        username: session.username.clone(),
        user_id: session.user_id.to_string(),
        access_token,
        refresh_token,
        scopes: session.scopes.clone(),
        is_waiting_for_second_factor_code: session.is_waiting_for_second_factor_code,
        password_mode: session.password_mode,
    };

    let writer = to_string_pretty(&stored, PrettyConfig::default())?;
    fs::write(path, writer)?;
    Ok(())
}

pub fn clear_persisted_session() {
    if let Ok(paths) = resolve_paths() {
        let _ = fs::remove_file(paths.credentials_path);
    } else {
        let _ = fs::remove_file(PathBuf::from("cred.ron"));
    }
}

pub async fn try_resume_session() -> Result<Option<(ProtonAPISession, String)>> {
    let cred_path = credentials_path();
    let Some(stored) = load_credentials(&cred_path)? else {
        return Ok(None);
    };

    let cache_repo = load_cache_repository()?;
    let mut resumed = ProtonAPISession::resume(
        SessionId::new(stored.session_id),
        stored.username,
        UserId::new(stored.user_id),
        stored.access_token,
        stored.refresh_token,
        stored.scopes,
        stored.is_waiting_for_second_factor_code,
        stored.password_mode,
        semver::Version::new(0, 1, 0),
        cache_repo,
    );

    match resumed.ensure_authenticated().await {
        Ok(()) => {
            persist_credentials(&cred_path, &resumed).await?;
            let display_name = resumed.username.clone();
            Ok(Some((resumed, display_name)))
        }
        Err(error) => {
            println!("Stored session expired or invalid ({error}). Please login again.");
            clear_persisted_session();
            Ok(None)
        }
    }
}

pub async fn authenticate() -> Result<(ProtonAPISession, String)> {
    let theme = ColorfulTheme::default();

    let username: String = Input::with_theme(&theme)
        .with_prompt("Username")
        .interact_text()?;

    let password: String = Password::with_theme(&theme)
        .with_prompt("Password")
        .interact()?;

    let cache_repo = load_cache_repository()?;
    let spinner = ProgressBar::new_spinner();
    spinner.set_message("Authenticating...");
    spinner.enable_steady_tick(Duration::from_millis(100));

    let mut session = ProtonAPISession::begin(
        &username,
        &password,
        semver::Version::new(0, 1, 0),
        ProtonSessionOptions::new(ProtonClientOptions {
            secret_cache_repository: Some(cache_repo),
            ..Default::default()
        }),
    )
    .await?;

    if session.is_waiting_for_second_factor_code {
        spinner.finish_and_clear();
        let code: String = Input::with_theme(&theme)
            .with_prompt("2FA Code")
            .interact_text()?;
        let spinner = ProgressBar::new_spinner();
        spinner.set_message("Authenticating...");
        spinner.enable_steady_tick(Duration::from_millis(100));
        session.apply_second_factor_code(code).await?;
        spinner.finish_and_clear();
    } else {
        spinner.finish_and_clear();
    }

    if let Err(e) = session.apply_data_password(&password).await {
        log::warn!("Failed to apply data password: {e}");
    }

    persist_credentials(&credentials_path(), &session).await?;

    let display_name = session.username.clone();
    println!("\n Authenticated as: {}", display_name);

    Ok((session, display_name))
}

