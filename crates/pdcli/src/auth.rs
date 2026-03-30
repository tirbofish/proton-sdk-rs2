use std::{
    fs,
    path::Path,
    sync::Arc,
};

use proton_drive_sdk::cache::sqlite::SqliteCacheRepository;
use proton_sdk_rs2::{
    PasswordMode, SessionId, UserId,
    client::ProtonClientOptions,
    session::{ProtonAPISession, ProtonSessionOptions},
};
use ron::ser::{PrettyConfig, to_string_pretty};

pub async fn authenticate() -> anyhow::Result<ProtonAPISession> {
    let app_version = semver::Version::new(0, 1, 0);
    let cred_path = crate::app_paths::cred_path();
    let stored = load_credentials(&cred_path)?;
    let settings = crate::settings::Settings::load().unwrap_or_default();

    std::fs::create_dir_all(crate::app_paths::app_dir())?;
    let entity_cache = Arc::new(SqliteCacheRepository::open_file(
        crate::app_paths::entity_cache_path(),
        settings.entity_cache_max_size,
    )?);
    let secret_cache = Arc::new(SqliteCacheRepository::open_file(
        crate::app_paths::secret_cache_path(),
        settings.secret_cache_max_size,
    )?);

    let session = if let Some(stored) = stored {
        let pb = crate::ui::spinner("Resuming stored session…");

        let mut resumed = ProtonAPISession::resume_with_options(
            SessionId::new(stored.session_id),
            stored.username,
            UserId::new(stored.user_id),
            stored.access_token,
            stored.refresh_token,
            stored.scopes,
            stored.is_waiting_for_second_factor_code,
            stored.password_mode,
            app_version.clone(),
            secret_cache,
            ProtonClientOptions {
                entity_cache_repository: Some(entity_cache),
                ..Default::default()
            },
        );

        if let Ok((access_token, _)) = resumed.token_credential.get_tokens().await {
            let _ = resumed
                .token_credential
                .get_refreshed_access_token(access_token)
                .await;
        }

        resumed.ensure_authenticated().await?;
        pb.finish_with_message("Session resumed");
        resumed
    } else {
        println!("No credentials found, creating a new session");
        begin_new_session(app_version.clone(), entity_cache, secret_cache).await?
    };

    persist_credentials(&cred_path, &session).await?;

    Ok(session)
}

async fn begin_new_session(
    app_version: semver::Version,
    entity_cache: Arc<SqliteCacheRepository>,
    secret_cache: Arc<SqliteCacheRepository>,
) -> anyhow::Result<ProtonAPISession> {
    let username = prompt_input("Username")?;
    let password = rpassword::prompt_password("Password: ")?;

    let mut session = ProtonAPISession::begin(
        &username,
        &password,
        app_version,
        ProtonSessionOptions::new(ProtonClientOptions {
            entity_cache_repository: Some(entity_cache),
            secret_cache_repository: Some(secret_cache),
            ..Default::default()
        }),
    )
    .await?;

    if session.is_waiting_for_second_factor_code {
        let mut line = String::new();
        println!("2FA code:");
        std::io::stdin().read_line(&mut line)?;
        session.apply_second_factor_code(line.trim().to_string()).await?;
    }

    if let Err(e) = session.apply_data_password(&password).await {
        log::warn!("Failed to apply data password: {e}");
    }

    Ok(session)
}

pub fn prompt_input(label: &str) -> anyhow::Result<String> {
    println!("{label}:");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let value = line.trim().to_string();
    if value.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{label} cannot be empty"),
        )
        .into());
    }
    Ok(value)
}

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

impl StoredCredentials {
    fn from_ron(content: &str) -> anyhow::Result<Self> {
        Ok(ron::from_str::<StoredCredentials>(content)?)
    }
}

fn load_credentials(path: &Path) -> anyhow::Result<Option<StoredCredentials>> {
    match fs::read_to_string(path) {
        Ok(content) => match StoredCredentials::from_ron(&content) {
            Ok(stored) => Ok(Some(stored)),
            Err(e) => {
                println!("Ignoring invalid credentials ({e}), starting fresh");
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub async fn persist_credentials(path: &Path, session: &ProtonAPISession) -> anyhow::Result<()> {
    let (access_token, refresh_token) = session.token_credential.get_tokens().await?;
    let creds = StoredCredentials {
        session_id: session.session_id.to_string(),
        username: session.username.clone(),
        user_id: session.user_id.to_string(),
        access_token,
        refresh_token,
        scopes: session.scopes.clone(),
        is_waiting_for_second_factor_code: session.is_waiting_for_second_factor_code,
        password_mode: session.password_mode,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, to_string_pretty(&creds, PrettyConfig::default())?)?;
    Ok(())
}

pub fn clear_credentials() -> anyhow::Result<()> {
    let cred = crate::app_paths::cred_path();
    if cred.exists() {
        fs::remove_file(&cred)?;
    }
    Ok(())
}

pub fn clear_all_data() -> anyhow::Result<()> {
    clear_credentials()?;
    for path in [
        crate::app_paths::entity_cache_path(),
        crate::app_paths::secret_cache_path(),
    ] {
        if path.exists() {
            fs::remove_file(&path)?;
        }
    }
    // Remove old cache.json if present from a previous install.
    let old_cache = crate::app_paths::cache_path();
    if old_cache.exists() {
        fs::remove_file(&old_cache)?;
    }
    Ok(())
}
